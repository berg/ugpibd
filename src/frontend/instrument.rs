// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// One GPIB instrument on the shared bus, as split primitives.
//
// `write` and `read` are separate operations here on purpose. Fusing them —
// deciding at write time whether a read follows — is a *protocol* concern,
// forced on HiSLIP by its push model, and it is exactly the decision that
// cannot be made correctly for instruments that only produce output once
// addressed to talk (an HP 8594E's `PRINT` sets no status bit; the data does
// not exist until the instrument is addressed). Front-ends whose wire
// protocol carries an explicit read request get to keep that honesty by
// calling `read` when — and only when — the client asked for one.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{broadcast, Mutex, MutexGuard};

use crate::backend::GpibBackend;

/// Upper bound on a single bulk read. Matches the Prologix server's default so
/// behavior is consistent across front-ends.
const MAX_READ: usize = 65536;

/// A GPIB instrument addressable at `pad` on the shared bus.
pub struct Instrument {
    ctrl: Arc<Mutex<dyn GpibBackend>>,
    pad: u8,
}

impl Instrument {
    pub fn new(ctrl: Arc<Mutex<dyn GpibBackend>>, pad: u8) -> Self {
        Self { ctrl, pad }
    }

    pub fn pad(&self) -> u8 {
        self.pad
    }

    /// The instrument, not the session or the protocol, is what a lock
    /// protects: two clients that resolve to the same primary address contend,
    /// and two that resolve to different ones do not — across front-ends.
    /// Every spelling that means the same PAD lands on the same key.
    pub fn resource_key(&self) -> String {
        format!("gpib{}", self.pad)
    }

    /// Take the bus for a multi-step transaction. Everything done through the
    /// returned guard is one uninterrupted bus tenure: no other client's
    /// traffic can interleave between, say, a write and the serial poll that
    /// observes its consequences. Single operations can use the one-shot
    /// methods below instead.
    pub async fn hold(&self) -> Held<'_> {
        Held {
            ctrl: self.ctrl.lock().await,
            pad: self.pad,
        }
    }

    /// Send a GPIB trigger (GET) to the instrument.
    pub async fn trigger(&self) -> Result<()> {
        self.hold().await.ctrl.trigger(self.pad).await
    }

    /// Send Selected Device Clear to the instrument.
    pub async fn device_clear(&self) -> Result<()> {
        self.hold().await.ctrl.device_clear(self.pad).await
    }

    /// Drive REN on/off. Bus-wide by nature of the line.
    pub async fn ren(&self, enable: bool) -> Result<()> {
        self.hold().await.ctrl.ren(enable).await
    }

    /// Assert REN and address this instrument, putting it into remote state.
    pub async fn go_to_remote(&self) -> Result<()> {
        self.hold().await.ctrl.go_to_remote(self.pad).await
    }

    /// Send an addressed Go To Local to this instrument.
    pub async fn go_to_local(&self) -> Result<()> {
        self.hold().await.ctrl.go_to_local(self.pad).await
    }

    /// Send Local Lockout, which the standard defines only bus-wide.
    pub async fn local_lockout(&self) -> Result<()> {
        self.hold().await.ctrl.local_lockout().await
    }

    /// Read the instrument's serial-poll status byte.
    pub async fn serial_poll(&self) -> Result<u8> {
        self.hold().await.ctrl.serial_poll(self.pad).await
    }

    /// Whether any device on the bus is currently asserting SRQ — a level
    /// read, not an event.
    pub async fn srq_asserted(&self) -> Result<bool> {
        self.hold().await.ctrl.srq_asserted().await
    }

    /// Subscribe to service requests. `None` means the underlying adapter has
    /// no way to report SRQ.
    pub async fn subscribe_srq(&self) -> Option<broadcast::Receiver<()>> {
        self.hold().await.ctrl.subscribe_srq()
    }
}

/// The bus, held. Operations are the same as [`Instrument`]'s plus the data
/// path; the difference is tenure — nothing else touches the bus until this
/// is dropped.
pub struct Held<'a> {
    ctrl: MutexGuard<'a, dyn GpibBackend>,
    pad: u8,
}

impl Held<'_> {
    /// Subscribe to service requests while already on the bus, so a request
    /// raised by traffic later in this same tenure is in the queue by the
    /// time it is checked.
    pub fn subscribe_srq(&self) -> Option<broadcast::Receiver<()>> {
        self.ctrl.subscribe_srq()
    }

    /// Write `data` to the instrument, EOI on the last byte if `send_eoi`.
    pub async fn write(&mut self, data: &[u8], send_eoi: bool) -> Result<()> {
        self.ctrl.write(self.pad, data, send_eoi).await
    }

    /// Address the instrument to talk and read until EOI, the byte limit, or
    /// the bus timeout. Returns the data and whether END (EOI) terminated it.
    pub async fn read(&mut self) -> Result<(Vec<u8>, bool)> {
        self.ctrl.read(self.pad, MAX_READ).await
    }

    /// Read the instrument's serial-poll status byte.
    pub async fn serial_poll(&mut self) -> Result<u8> {
        self.ctrl.serial_poll(self.pad).await
    }

    /// Whether any device on the bus is currently asserting SRQ.
    pub async fn srq_asserted(&mut self) -> Result<bool> {
        self.ctrl.srq_asserted().await
    }
}
