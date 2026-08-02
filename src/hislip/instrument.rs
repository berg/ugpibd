// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Bridge from the HiSLIP `Device` abstraction to our `GpibController`.

use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::Mutex;

use tracing::debug;

use super::server::{Device, Execution, STB_MAV, STB_RQS};
use crate::backend::GpibBackend;

/// Did a service request arrive on this subscription? A lagged channel counts:
/// SRQ is a level rather than a tally, so a missed notification and a delivered
/// one mean the same thing.
fn srq_fired(srq: &mut Option<tokio::sync::broadcast::Receiver<()>>) -> bool {
    use tokio::sync::broadcast::error::TryRecvError;
    matches!(
        srq.as_mut().map(|rx| rx.try_recv()),
        Some(Ok(())) | Some(Err(TryRecvError::Lagged(_)))
    )
}

/// A GPIB instrument addressable at `pad` on the shared bus.
pub struct GpibInstrument {
    ctrl: Arc<Mutex<dyn GpibBackend>>,
    pad: u8,
    /// Upper bound on a single bulk read. Matches the Prologix server's
    /// default so behavior is consistent across front-ends.
    max_read: usize,
}

impl GpibInstrument {
    pub fn new(ctrl: Arc<Mutex<dyn GpibBackend>>, pad: u8) -> Self {
        Self {
            ctrl,
            pad,
            max_read: 65536,
        }
    }
}

#[async_trait::async_trait]
impl Device for GpibInstrument {
    /// Run one command, and along the way notice any service request the
    /// instrument raises because of it.
    ///
    /// The noticing has to happen here, because this is the only code that
    /// holds the bus across the window in which such a request exists. HiSLIP
    /// has no read request — the server pushes replies — so a GPIB bridge must
    /// drain the instrument's output queue on its own initiative, and that read
    /// is exactly what clears MAV. Whoever looks afterwards sees nothing.
    ///
    /// Note what is *not* done: nothing here inspects the command for `*SRE`.
    /// Applying that mask is the instrument's job — it computes
    /// `RQS = OR(STB & SRE)` and pulls the SRQ line itself — and a copy of the
    /// mask sniffed out of the byte stream would be a cache with no
    /// invalidation, blind to the front panel, to another controller, to `*PSC`
    /// and to the several NRf spellings of the same number. The SRQ line is the
    /// instrument's own answer, so that is what we watch.
    async fn execute(&self, cmd: &[u8], expect_response: bool) -> Result<Execution> {
        let mut ctrl = self.ctrl.lock().await;
        // Subscribe before touching the bus, so a request raised anywhere from
        // here on is in the queue by the time we look.
        let mut srq = ctrl.subscribe_srq();

        ctrl.write(self.pad, cmd, true).await?;
        if !expect_response {
            return Ok(Execution::default());
        }

        // Poll before draining, not after. If the instrument is already asking
        // for service this is the one moment its own status byte can still be
        // read; afterwards the condition is gone. Measured on an HP 34401A:
        // SRQ comes up 5-13 ms after the write, and the poll returns 0x50.
        let mut service_request = None;
        if ctrl.srq_asserted().await.unwrap_or(false) {
            match ctrl.serial_poll(self.pad).await {
                Ok(stb) if stb & STB_RQS != 0 => service_request = Some(stb),
                // SRQ is a wired-OR: somebody else on the bus is holding it.
                Ok(_) => {}
                Err(e) => debug!("serial poll before read failed: {e:#}"),
            }
        }

        let (data, end) = ctrl.read(self.pad, self.max_read).await?;
        // Nothing back *and* no END is not an empty answer: the instrument was
        // addressed to talk and had nothing to say, which is what a query it
        // does not implement looks like from here (an HP 34401A asked `*LRN?`
        // answers -113 and stays silent). Report that rather than hand the
        // caller an empty string it cannot tell from a real one — a plausible
        // lie is worse than a loud failure. A device that genuinely returns an
        // empty message asserts END with it, and that still comes back as one.
        if data.is_empty() && !end {
            bail!("instrument returned no data (is the query supported?)");
        }

        // The MAV case, and the reason this function exists in this shape: the
        // instrument raised SRQ while we were on the bus, and the read above
        // took the very output that caused it. There is nothing left to poll,
        // so the status byte is reconstructed — but the *decision* is still the
        // instrument's, since it only pulled the line because its own service
        // request mask said to.
        if service_request.is_none() && !data.is_empty() && srq_fired(&mut srq) {
            match ctrl.srq_asserted().await {
                // Still asserted, so it outlived our read and was not about
                // the output we took. Leave it: the session's SRQ forwarder
                // will poll it and report the real byte.
                Ok(true) => {}
                Ok(false) => {
                    debug!("service request cleared by our own read; reporting MAV");
                    service_request = Some(STB_RQS | STB_MAV);
                }
                Err(e) => debug!("cannot tell whether srq survived the read: {e:#}"),
            }
        }

        Ok(Execution {
            data: Some(data),
            service_request,
        })
    }

    async fn trigger(&self) -> Result<()> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.trigger(self.pad).await
    }

    async fn clear(&self) -> Result<()> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.device_clear(self.pad).await
    }

    async fn set_remote(&self, remote: bool) -> Result<()> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.ren(remote).await
    }

    async fn go_to_remote(&self) -> Result<()> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.go_to_remote(self.pad).await
    }

    async fn go_to_local(&self) -> Result<()> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.go_to_local(self.pad).await
    }

    async fn local_lockout(&self) -> Result<()> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.local_lockout().await
    }

    async fn get_status(&self) -> Result<u8> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.serial_poll(self.pad).await
    }

    async fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        let ctrl = self.ctrl.lock().await;
        ctrl.subscribe_srq()
    }

    async fn srq_asserted(&self) -> Result<bool> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.srq_asserted().await
    }

    /// The instrument, not the session, is what a lock protects: two clients
    /// that resolve to the same primary address contend, and two that resolve
    /// to different ones do not. Sub-address spellings that mean the same PAD
    /// (`hislip23`, `gpib0,23`, and `hislip0` when 23 is the default) all land
    /// on the same key, because the PAD is resolved before we get here.
    fn resource_key(&self) -> String {
        format!("gpib{}", self.pad)
    }
}
