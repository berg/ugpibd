// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// USBTMC / USB488 backend: any instrument that speaks the USB Test &
// Measurement Class directly — a scope or DMM's own USB port, or a GPIB bridge
// that presents its instrument as a USB488 device (xyphro's UsbGpib).
//
// USB488 was designed as IEEE-488 carried over USB, so the trait maps almost
// one to one: DEV_DEP_MSG_OUT is `write`, REQUEST_DEV_DEP_MSG_IN plus the
// bulk-IN reply is `read`, INITIATE_CLEAR is `device_clear`, the TRIGGER
// message is `trigger`, READ_STATUS_BYTE is `serial_poll`, and REN_CONTROL,
// GO_TO_LOCAL and LOCAL_LOCKOUT are the remote/local operations. Service
// requests arrive on the interface's interrupt-IN endpoint.
//
// What does not map is the bus. A USB488 interface *is* one instrument: there
// is no primary address on the wire, so the `pad` every trait method carries
// is accepted and ignored — `gpib0,5` and `hislip0` reach the same device.
// Nothing here can read control lines, act as a device, listen unaddressed or
// send raw command bytes; those keep the trait's refusing defaults, and `ifc`
// reduces to putting our own half-finished transfers right.
//
// Kernel drivers bind these interfaces (`usbtmc` on Linux, nothing on macOS),
// and the transport detaches it from the one interface it opens.
//
// Not derived from the kernel driver: written against the USBTMC 1.0 and
// USB488 1.0 specifications, with `drivers/usb/class/usbtmc.c` consulted for
// how a real driver sequences the abort and clear handshakes.

pub mod protocol;
pub mod usb;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::backend::{GpibBackend, SharedBackend};
use protocol::*;

/// Backend identifier used by `--backend`.
pub const ID: &str = "usbtmc";

/// Human-readable description shown by `--backend list`.
pub const DESCRIPTION: &str = "any USBTMC/USB488 instrument (USB class fe/03; untested)";

/// Cap on one REQUEST_DEV_DEP_MSG_IN. Front-ends ask for 64 KiB at a time, so
/// this only bounds what an unusual caller can make a device queue.
pub const MAX_READ_TRANSFER: usize = 1 << 20;

/// How many times to re-check an abort or clear handshake still reported as
/// PENDING before giving up on it.
const HANDSHAKE_POLLS: usize = 50;

/// Pause between handshake polls. Devices answer PENDING while they finish a
/// transfer internally; this is the spec's suggested order of magnitude.
const HANDSHAKE_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Size of the reads that empty a bulk-IN FIFO during abort and clear.
const DRAIN_READ: usize = 4096;

/// Longest wait for a READ_STATUS_BYTE reply on the interrupt endpoint before
/// falling back to the control reply's byte. A status byte is meant to arrive
/// at once; this only bounds the flaky case.
const STATUS_BYTE_WAIT_MS: u64 = 1000;

/// The USB operations the backend needs, abstracted so the message sequencing
/// and the abort/clear handshakes are unit-tested against a mock.
#[async_trait::async_trait]
pub trait TmcTransport: Send + Sync {
    /// One bulk-OUT transfer, the whole of `data`.
    async fn bulk_out(&self, data: &[u8]) -> Result<()>;

    /// One bulk-IN transfer of at most `max_len` bytes: everything the device
    /// sends up to the short packet that ends the transfer. On a timeout the
    /// transport must leave nothing of its own queued on the endpoint.
    async fn bulk_in(&self, max_len: usize) -> Result<Vec<u8>>;

    /// A class control-IN request. `index` is the interface number or the
    /// endpoint address, as the recipient demands.
    async fn control_in(
        &self,
        recipient: Recipient,
        request: u8,
        value: u16,
        index: u16,
        len: usize,
    ) -> Result<Vec<u8>>;

    /// Discard anything queued on the bulk-IN endpoint and clear its halt.
    async fn reset_bulk_in(&self) -> Result<()>;

    /// Likewise for bulk-OUT.
    async fn reset_bulk_out(&self) -> Result<()>;

    fn interface_number(&self) -> u8;
    fn bulk_in_address(&self) -> u8;
    fn bulk_out_address(&self) -> u8;

    /// How long a bulk transfer may wait before it is a failure.
    fn set_timeout(&self, timeout_ms: u32);

    /// Whether the interface has an interrupt-IN endpoint. When it does, USB488
    /// delivers READ_STATUS_BYTE replies and service requests there.
    fn has_interrupt_in(&self) -> bool {
        false
    }

    /// Wait up to `wait` for the READ_STATUS_BYTE reply tagged `tag` on the
    /// interrupt endpoint and return the status byte it carried.
    ///
    /// Bounded separately from the GPIB timeout because a status byte is
    /// meant to arrive at once: waiting the whole timeout on a device that
    /// never delivers it — a Rigol DHO800 is one — would stall every poll.
    async fn status_byte_notification(&self, tag: u8, wait: Duration) -> Result<u8> {
        let _ = (tag, wait);
        bail!("this interface has no interrupt endpoint")
    }

    /// Service-request notifications, when the interrupt endpoint exists.
    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        None
    }
}

/// One USBTMC interface, driven as if it were a GPIB instrument.
pub struct UsbtmcBackend<T: TmcTransport> {
    transport: T,
    caps: Capabilities,
    /// bTag of the last bulk message sent.
    tag: u8,
    /// bTag of the last READ_STATUS_BYTE.
    status_tag: u8,
    /// A bulk-OUT message that may not have been fully accepted, by bTag.
    /// Set before the transfer and cleared when it completes, so a future
    /// dropped mid-transfer leaves it behind for the next call to abort.
    pending_out: Option<u8>,
    /// A REQUEST_DEV_DEP_MSG_IN whose reply was not fully taken, likewise.
    pending_in: Option<u8>,
    my_pad: u8,
    eos_char: u8,
    eos_enabled: bool,
    timeout_ms: u32,
}

impl<T: TmcTransport> UsbtmcBackend<T> {
    pub fn new(transport: T, timeout_ms: u32) -> Self {
        transport.set_timeout(timeout_ms);
        Self {
            transport,
            caps: Capabilities::default(),
            tag: 0,
            status_tag: 1,
            pending_out: None,
            pending_in: None,
            my_pad: 0,
            eos_char: b'\n',
            eos_enabled: false,
            timeout_ms,
        }
    }

    /// What the interface advertised at init.
    pub fn capabilities(&self) -> Capabilities {
        self.caps
    }

    /// How long to wait for a status byte on the interrupt endpoint: capped
    /// well below the GPIB timeout so a device that never delivers it does not
    /// stall every poll for the full timeout.
    fn status_byte_wait(&self) -> Duration {
        Duration::from_millis(u64::from(self.timeout_ms).min(STATUS_BYTE_WAIT_MS))
    }

    fn take_tag(&mut self) -> u8 {
        self.tag = next_tag(self.tag);
        self.tag
    }

    async fn interface_request(&self, request: u8, value: u16, len: usize) -> Result<Vec<u8>> {
        self.transport
            .control_in(
                Recipient::Interface,
                request,
                value,
                u16::from(self.transport.interface_number()),
                len,
            )
            .await
    }

    async fn endpoint_request(
        &self,
        request: u8,
        value: u16,
        endpoint: u8,
        len: usize,
    ) -> Result<Vec<u8>> {
        self.transport
            .control_in(
                Recipient::Endpoint,
                request,
                value,
                u16::from(endpoint),
                len,
            )
            .await
    }

    fn require_usb488(&self, what: &str) -> Result<()> {
        if self.caps.bcd_usb488 == 0 {
            bail!("{what}: this is a plain USBTMC interface, not USB488");
        }
        Ok(())
    }

    fn require_remote_local(&self, what: &str) -> Result<()> {
        self.require_usb488(what)?;
        if !self.caps.remote_local {
            bail!(
                "{what}: the device does not advertise REN_CONTROL / GO_TO_LOCAL / LOCAL_LOCKOUT"
            );
        }
        Ok(())
    }

    /// Put right anything an earlier call left half-done before the bulk pipe
    /// is used again. A read that timed out, or a future dropped mid-transfer,
    /// leaves the device with a message in flight; the next request would then
    /// be answered with the *previous* reply, for the life of the session.
    ///
    /// The spec's per-endpoint abort handshake is tried first. If it fails the
    /// heavier INITIATE_CLEAR is used, which also empties the instrument's
    /// buffers. If that fails too the error is returned and the flags stay
    /// set, so the next call tries again rather than proceeding on a pipe
    /// known to be out of step.
    async fn settle(&mut self) -> Result<()> {
        if self.pending_in.is_none() && self.pending_out.is_none() {
            return Ok(());
        }
        let mut abort = Ok(());
        if let Some(tag) = self.pending_in {
            abort = self.abort_bulk_in(tag).await;
        }
        if let Some(tag) = self.pending_out {
            if abort.is_ok() {
                abort = self.abort_bulk_out(tag).await;
            }
        }
        if let Err(e) = abort {
            warn!("usbtmc abort handshake failed ({e:#}); falling back to INITIATE_CLEAR");
            self.clear_device()
                .await
                .context("recovering the bulk pipe")?;
        }
        self.pending_in = None;
        self.pending_out = None;
        Ok(())
    }

    /// Best-effort settle after a failed transfer, keeping the original error.
    async fn fail<V>(&mut self, err: anyhow::Error) -> Result<V> {
        if let Err(e) = self.settle().await {
            warn!("usbtmc: pipe not yet recovered after a failed transfer: {e:#}");
        }
        Err(err)
    }

    /// USBTMC §4.2.1.4–5: abort the bulk-IN transfer tagged `tag`, emptying
    /// the device's FIFO as it asks.
    async fn abort_bulk_in(&mut self, tag: u8) -> Result<()> {
        let ep = self.transport.bulk_in_address();
        let reply = self
            .endpoint_request(REQ_INITIATE_ABORT_BULK_IN, u16::from(tag), ep, 2)
            .await
            .context("INITIATE_ABORT_BULK_IN")?;
        match reply.first().copied() {
            Some(STATUS_SUCCESS) => {
                // Our own stale transfer first, then whatever the device
                // still holds, until it agrees the abort is complete.
                self.transport.reset_bulk_in().await?;
                for _ in 0..HANDSHAKE_POLLS {
                    let status = self
                        .endpoint_request(REQ_CHECK_ABORT_BULK_IN_STATUS, 0, ep, 8)
                        .await
                        .context("CHECK_ABORT_BULK_IN_STATUS")?;
                    match status.first().copied() {
                        Some(STATUS_SUCCESS) => break,
                        Some(STATUS_PENDING) => {
                            if status.get(1).is_some_and(|b| b & 0x01 != 0) {
                                let _ = self.transport.bulk_in(DRAIN_READ).await;
                            } else {
                                tokio::time::sleep(HANDSHAKE_POLL_INTERVAL).await;
                            }
                        }
                        _ => return check_status("CHECK_ABORT_BULK_IN_STATUS", &status),
                    }
                }
                self.transport.reset_bulk_in().await
            }
            // Nothing in flight on the device's side: only ours to tidy.
            Some(STATUS_TRANSFER_NOT_IN_PROGRESS) => self.transport.reset_bulk_in().await,
            _ => check_status("INITIATE_ABORT_BULK_IN", &reply),
        }
    }

    /// USBTMC §4.2.1.2–3: abort the bulk-OUT transfer tagged `tag`.
    async fn abort_bulk_out(&mut self, tag: u8) -> Result<()> {
        let ep = self.transport.bulk_out_address();
        let reply = self
            .endpoint_request(REQ_INITIATE_ABORT_BULK_OUT, u16::from(tag), ep, 2)
            .await
            .context("INITIATE_ABORT_BULK_OUT")?;
        match reply.first().copied() {
            Some(STATUS_SUCCESS) => {
                for _ in 0..HANDSHAKE_POLLS {
                    let status = self
                        .endpoint_request(REQ_CHECK_ABORT_BULK_OUT_STATUS, 0, ep, 8)
                        .await
                        .context("CHECK_ABORT_BULK_OUT_STATUS")?;
                    match status.first().copied() {
                        Some(STATUS_SUCCESS) => break,
                        Some(STATUS_PENDING) => {
                            tokio::time::sleep(HANDSHAKE_POLL_INTERVAL).await;
                        }
                        _ => return check_status("CHECK_ABORT_BULK_OUT_STATUS", &status),
                    }
                }
                self.transport.reset_bulk_out().await
            }
            Some(STATUS_TRANSFER_NOT_IN_PROGRESS) => self.transport.reset_bulk_out().await,
            _ => check_status("INITIATE_ABORT_BULK_OUT", &reply),
        }
    }

    /// USBTMC §4.2.1.6–7: INITIATE_CLEAR, then CHECK_CLEAR_STATUS until the
    /// device is done, reading out its bulk-IN FIFO when it says to, then the
    /// bulk-OUT halt clear the spec requires afterwards.
    async fn clear_device(&mut self) -> Result<()> {
        let reply = self
            .interface_request(REQ_INITIATE_CLEAR, 0, 1)
            .await
            .context("INITIATE_CLEAR")?;
        check_status("INITIATE_CLEAR", &reply)?;
        let mut done = false;
        for _ in 0..HANDSHAKE_POLLS {
            let status = self
                .interface_request(REQ_CHECK_CLEAR_STATUS, 0, 2)
                .await
                .context("CHECK_CLEAR_STATUS")?;
            match status.first().copied() {
                Some(STATUS_SUCCESS) => {
                    done = true;
                    break;
                }
                Some(STATUS_PENDING) => {
                    if status.get(1).is_some_and(|b| b & 0x01 != 0) {
                        let _ = self.transport.bulk_in(DRAIN_READ).await;
                    } else {
                        tokio::time::sleep(HANDSHAKE_POLL_INTERVAL).await;
                    }
                }
                _ => return check_status("CHECK_CLEAR_STATUS", &status),
            }
        }
        if !done {
            bail!("usbtmc CHECK_CLEAR_STATUS still PENDING after {HANDSHAKE_POLLS} polls");
        }
        self.transport.reset_bulk_in().await?;
        self.transport.reset_bulk_out().await?;
        self.pending_in = None;
        self.pending_out = None;
        Ok(())
    }

    async fn send_message(&mut self, what: &str, msg: Vec<u8>, tag: u8) -> Result<()> {
        self.settle().await?;
        self.pending_out = Some(tag);
        match self.transport.bulk_out(&msg).await {
            Ok(()) => {
                self.pending_out = None;
                Ok(())
            }
            Err(e) => self.fail(e.context(format!("usbtmc {what}"))).await,
        }
    }

    async fn write_message(&mut self, data: &[u8], eom: bool) -> Result<()> {
        let tag = self.take_tag();
        debug!(tag, len = data.len(), eom, "usbtmc DEV_DEP_MSG_OUT");
        self.send_message("write", encode_dev_dep_msg_out(tag, data, eom), tag)
            .await
    }

    async fn read_message(&mut self, max_len: usize) -> Result<(Vec<u8>, bool)> {
        let want = max_len.clamp(1, MAX_READ_TRANSFER);
        let term_char = (self.eos_enabled && self.caps.term_char).then_some(self.eos_char);
        let tag = self.take_tag();
        debug!(tag, want, ?term_char, "usbtmc REQUEST_DEV_DEP_MSG_IN");
        self.send_message(
            "read request",
            encode_request_dev_dep_msg_in(tag, want as u32, term_char),
            tag,
        )
        .await?;

        self.pending_in = Some(tag);
        let buf = match self
            .transport
            .bulk_in(HEADER_LEN + want + pad_len(want))
            .await
        {
            Ok(buf) => buf,
            Err(e) => return self.fail(e.context("usbtmc read")).await,
        };
        let msg = match parse_dev_dep_msg_in(&buf, tag) {
            Ok(msg) => msg,
            // Garbled, short, or an answer to an earlier request: `pending_in`
            // is still set, so recovery aborts bulk-IN, which also drains a
            // stale reply sitting in the pipe. Only if that abort itself fails
            // does it escalate to INITIATE_CLEAR — the heavier reset that a
            // fragile device is happiest not seeing on every unsupported query.
            Err(e) => return self.fail(e).await,
        };
        self.pending_in = None;
        if msg.data.len() < msg.transfer_size {
            warn!(
                got = msg.data.len(),
                claimed = msg.transfer_size,
                "usbtmc bulk-in transfer ended short of its own header"
            );
        }
        debug!(
            tag,
            len = msg.data.len(),
            eom = msg.eom,
            term = msg.term_char_seen,
            "usbtmc DEV_DEP_MSG_IN"
        );
        Ok((msg.data, msg.eom || msg.term_char_seen))
    }
}

#[async_trait::async_trait]
impl<T: TmcTransport> GpibBackend for UsbtmcBackend<T> {
    async fn init(&mut self, my_pad: u8) -> Result<()> {
        self.my_pad = my_pad;
        let reply = self
            .interface_request(REQ_GET_CAPABILITIES, 0, 24)
            .await
            .context("GET_CAPABILITIES")?;
        self.caps = parse_capabilities(&reply)?;
        info!(
            usbtmc = format!("{:x}", self.caps.bcd_usbtmc),
            usb488 = format!("{:x}", self.caps.bcd_usb488),
            term_char = self.caps.term_char,
            remote_local = self.caps.remote_local,
            trigger = self.caps.trigger,
            sr1 = self.caps.sr1,
            interrupt_in = self.transport.has_interrupt_in(),
            "usbtmc interface capabilities"
        );
        if self.caps.bcd_usb488 == 0 {
            warn!("plain USBTMC interface: serial poll, trigger and remote/local control are unavailable");
        }
        // The equivalent of the IFC pulse the other backends start with:
        // abort whatever a dead predecessor left in flight and start from
        // empty buffers.
        self.clear_device().await.context("initial device clear")?;
        if self.caps.remote_local {
            if let Err(e) = self.ren(true).await {
                warn!("usbtmc: could not assert REN at init: {e:#}");
            }
        }
        Ok(())
    }

    async fn write(&mut self, _pad: u8, data: &[u8], send_eoi: bool) -> Result<()> {
        self.write_message(data, send_eoi).await
    }

    async fn read(&mut self, _pad: u8, max_len: usize) -> Result<(Vec<u8>, bool)> {
        self.read_message(max_len).await
    }

    async fn device_clear(&mut self, _pad: u8) -> Result<()> {
        self.clear_device().await
    }

    async fn trigger(&mut self, _pad: u8) -> Result<()> {
        self.require_usb488("trigger")?;
        if !self.caps.trigger {
            bail!("trigger: the device does not accept the USB488 TRIGGER message");
        }
        let tag = self.take_tag();
        self.send_message("trigger", encode_trigger(tag), tag).await
    }

    /// No bus to clear. What IFC does for a controller — abandon transfers in
    /// progress and return to idle — is done here for our own half-finished
    /// transfers, which is the only state there is.
    async fn ifc(&mut self) -> Result<()> {
        self.settle().await
    }

    async fn ren(&mut self, enable: bool) -> Result<()> {
        self.require_remote_local("REN")?;
        let reply = self
            .interface_request(REQ_REN_CONTROL, u16::from(enable), 1)
            .await
            .context("REN_CONTROL")?;
        check_status("REN_CONTROL", &reply)
    }

    /// USB488 §4.3.2.1: REN_CONTROL(1) puts the device into remote state
    /// directly, which is what addressing it as a listener does on a bus.
    async fn go_to_remote(&mut self, _pad: u8) -> Result<()> {
        self.ren(true).await
    }

    async fn go_to_local(&mut self, _pad: u8) -> Result<()> {
        self.require_remote_local("GTL")?;
        let reply = self
            .interface_request(REQ_GO_TO_LOCAL, 0, 1)
            .await
            .context("GO_TO_LOCAL")?;
        check_status("GO_TO_LOCAL", &reply)
    }

    async fn local_lockout(&mut self) -> Result<()> {
        self.require_remote_local("LLO")?;
        let reply = self
            .interface_request(REQ_LOCAL_LOCKOUT, 0, 1)
            .await
            .context("LOCAL_LOCKOUT")?;
        check_status("LOCAL_LOCKOUT", &reply)
    }

    /// USB488 §4.3.1.2: the status byte comes back in the control reply on an
    /// interface without an interrupt endpoint, and on the interrupt endpoint —
    /// tagged to match — when there is one.
    ///
    /// The spec calls the control reply's byte undefined when an interrupt
    /// endpoint is present, but a Rigol DHO800 delivers the interrupt late or
    /// not at all while filling that byte in anyway. So wait briefly for the
    /// interrupt and fall back to the control byte: a compliant device is read
    /// from the endpoint the spec names, and a flaky one still answers without
    /// stalling the whole timeout on every poll (the HiSLIP front-end polls
    /// after every write).
    async fn serial_poll(&mut self, _pad: u8) -> Result<u8> {
        self.require_usb488("serial poll")?;
        self.status_tag = next_status_tag(self.status_tag);
        let tag = self.status_tag;
        let reply = self
            .interface_request(REQ_READ_STATUS_BYTE, u16::from(tag), 3)
            .await
            .context("READ_STATUS_BYTE")?;
        check_status("READ_STATUS_BYTE", &reply)?;
        if reply.get(1) != Some(&tag) {
            bail!(
                "READ_STATUS_BYTE reply carries bTag {:?}, expected {tag}",
                reply.get(1)
            );
        }
        let control_byte = reply.get(2).copied();
        if self.transport.has_interrupt_in() {
            let wait = self.status_byte_wait();
            match self.transport.status_byte_notification(tag, wait).await {
                Ok(stb) => return Ok(stb),
                Err(e) => match control_byte {
                    Some(stb) => {
                        debug!(
                            "usbtmc: no status byte on the interrupt endpoint \
                             ({e:#}); using the control reply's {stb:#04x}"
                        );
                        return Ok(stb);
                    }
                    None => return Err(e).context("READ_STATUS_BYTE via the interrupt endpoint"),
                },
            }
        }
        control_byte.context("READ_STATUS_BYTE reply is missing the status byte")
    }

    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        self.transport.subscribe_srq()
    }

    fn controller_pad(&self) -> u8 {
        self.my_pad
    }

    async fn set_controller_pad(&mut self, pad: u8) -> Result<()> {
        self.my_pad = pad;
        Ok(())
    }

    /// There is no addressing to skip: the data path is the same.
    async fn send_data_unaddressed(&mut self, data: &[u8], send_eoi: bool) -> Result<()> {
        self.write_message(data, send_eoi).await
    }

    async fn read_unaddressed(&mut self, max_len: usize) -> Result<(Vec<u8>, bool)> {
        self.read_message(max_len).await
    }

    fn set_eos(&mut self, eos_char: u8, enabled: bool) {
        self.eos_char = eos_char;
        self.eos_enabled = enabled;
        if enabled && !self.caps.term_char {
            warn!(
                "usbtmc: the device does not support TermChar, so the EOS terminator \
                 {eos_char:#04x} will not end reads; only EOM will"
            );
        }
    }

    fn eos(&self) -> (u8, bool) {
        (self.eos_char, self.eos_enabled)
    }

    fn set_timeout(&mut self, timeout_ms: u32) {
        self.timeout_ms = timeout_ms;
        self.transport.set_timeout(timeout_ms);
    }

    fn name(&self) -> &'static str {
        ID
    }

    /// Hand the instrument back to its front panel.
    async fn shutdown(&mut self) -> Result<()> {
        if self.caps.remote_local {
            self.ren(false).await?;
        }
        Ok(())
    }
}

/// Discover, open, and initialize a USBTMC interface. `port` restricts the
/// search to the device at that USB port id.
pub async fn open(timeout_ms: u32, port: Option<&str>) -> Result<SharedBackend> {
    let transport = usb::UsbtmcTransport::open(port).await?;
    let mut backend = UsbtmcBackend::new(transport, timeout_ms);
    backend.init(0).await.context("usbtmc init")?;
    info!("usbtmc interface ready");
    Ok(Arc::new(Mutex::new(backend)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// A scripted device: records every bulk-OUT and control request, answers
    /// bulk-IN from a queue, and answers control requests from a fixed table.
    struct MockTransport {
        written: StdMutex<Vec<Vec<u8>>>,
        in_queue: StdMutex<Vec<Result<Vec<u8>, String>>>,
        controls: StdMutex<Vec<(Recipient, u8, u16, u16)>>,
        resets: StdMutex<Vec<&'static str>>,
        caps: Vec<u8>,
        stb: u8,
        interrupt: bool,
        /// Whether the interrupt endpoint actually delivers the notification.
        /// A Rigol DHO800 sometimes does not, which is the fallback path.
        deliver_notification: bool,
        /// Status byte the interrupt endpoint would deliver, keyed by tag.
        notified: StdMutex<Vec<(u8, u8)>>,
    }

    /// A full USB488 capabilities block: TermChar, 488.2, remote/local,
    /// trigger, SCPI, SR1, RL1, DT1.
    fn usb488_caps() -> Vec<u8> {
        let mut c = vec![1, 0, 0x00, 0x01, 0, 0x01, 0, 0, 0, 0, 0, 0];
        c.extend_from_slice(&[0x00, 0x01, 0x07, 0x0f]);
        c.extend_from_slice(&[0; 8]);
        c
    }

    /// A plain USBTMC block with no USB488 part and no TermChar.
    fn plain_caps() -> Vec<u8> {
        vec![1, 0, 0x00, 0x01, 0, 0x00, 0, 0, 0, 0, 0, 0]
    }

    impl MockTransport {
        fn new(caps: Vec<u8>) -> Self {
            Self {
                written: StdMutex::new(vec![]),
                in_queue: StdMutex::new(vec![]),
                controls: StdMutex::new(vec![]),
                resets: StdMutex::new(vec![]),
                caps,
                stb: 0x50,
                interrupt: false,
                deliver_notification: true,
                notified: StdMutex::new(vec![]),
            }
        }

        fn queue_in(&self, r: Result<Vec<u8>, &str>) {
            self.in_queue.lock().unwrap().push(r.map_err(str::to_owned));
        }

        fn requests(&self) -> Vec<u8> {
            self.controls.lock().unwrap().iter().map(|c| c.1).collect()
        }
    }

    #[async_trait::async_trait]
    impl TmcTransport for MockTransport {
        async fn bulk_out(&self, data: &[u8]) -> Result<()> {
            self.written.lock().unwrap().push(data.to_vec());
            Ok(())
        }
        async fn bulk_in(&self, _max_len: usize) -> Result<Vec<u8>> {
            let mut q = self.in_queue.lock().unwrap();
            if q.is_empty() {
                bail!("mock: bulk_in with nothing queued");
            }
            q.remove(0).map_err(|e| anyhow::anyhow!(e))
        }
        async fn control_in(
            &self,
            recipient: Recipient,
            request: u8,
            value: u16,
            index: u16,
            _len: usize,
        ) -> Result<Vec<u8>> {
            self.controls
                .lock()
                .unwrap()
                .push((recipient, request, value, index));
            Ok(match request {
                REQ_GET_CAPABILITIES => self.caps.clone(),
                REQ_CHECK_CLEAR_STATUS => vec![STATUS_SUCCESS, 0],
                REQ_READ_STATUS_BYTE => {
                    // The status byte is always in the control reply here; the
                    // interrupt notification is delivered too, unless the
                    // device is one that drops it.
                    if self.interrupt && self.deliver_notification {
                        self.notified.lock().unwrap().push((value as u8, self.stb));
                    }
                    vec![STATUS_SUCCESS, value as u8, self.stb]
                }
                REQ_INITIATE_ABORT_BULK_IN | REQ_INITIATE_ABORT_BULK_OUT => {
                    vec![STATUS_SUCCESS, value as u8]
                }
                REQ_CHECK_ABORT_BULK_IN_STATUS | REQ_CHECK_ABORT_BULK_OUT_STATUS => {
                    vec![STATUS_SUCCESS, 0, 0, 0, 0, 0, 0, 0]
                }
                _ => vec![STATUS_SUCCESS],
            })
        }
        async fn reset_bulk_in(&self) -> Result<()> {
            self.resets.lock().unwrap().push("in");
            Ok(())
        }
        async fn reset_bulk_out(&self) -> Result<()> {
            self.resets.lock().unwrap().push("out");
            Ok(())
        }
        fn interface_number(&self) -> u8 {
            1
        }
        fn bulk_in_address(&self) -> u8 {
            0x82
        }
        fn bulk_out_address(&self) -> u8 {
            0x02
        }
        fn set_timeout(&self, _timeout_ms: u32) {}
        fn has_interrupt_in(&self) -> bool {
            self.interrupt
        }
        async fn status_byte_notification(&self, tag: u8, _wait: Duration) -> Result<u8> {
            let mut n = self.notified.lock().unwrap();
            let pos = n
                .iter()
                .position(|(t, _)| *t == tag)
                .context("mock: no notification for that tag")?;
            Ok(n.remove(pos).1)
        }
    }

    fn in_transfer(tag: u8, data: &[u8], attrs: u8) -> Vec<u8> {
        let mut v = vec![MSGID_DEV_DEP_MSG_IN, tag, !tag, 0];
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&[attrs, 0, 0, 0]);
        v.extend_from_slice(data);
        v.extend(std::iter::repeat_n(0u8, pad_len(data.len())));
        v
    }

    async fn ready(caps: Vec<u8>) -> UsbtmcBackend<MockTransport> {
        let mut be = UsbtmcBackend::new(MockTransport::new(caps), 3000);
        be.init(0).await.unwrap();
        be.transport.controls.lock().unwrap().clear();
        be.transport.resets.lock().unwrap().clear();
        be
    }

    #[tokio::test]
    async fn init_reads_capabilities_clears_and_asserts_ren() {
        let mut be = UsbtmcBackend::new(MockTransport::new(usb488_caps()), 3000);
        be.init(0).await.unwrap();
        assert_eq!(
            be.transport.requests(),
            vec![
                REQ_GET_CAPABILITIES,
                REQ_INITIATE_CLEAR,
                REQ_CHECK_CLEAR_STATUS,
                REQ_REN_CONTROL
            ]
        );
        let ren = be.transport.controls.lock().unwrap()[3];
        assert_eq!(ren, (Recipient::Interface, REQ_REN_CONTROL, 1, 1));
        assert!(be.capabilities().sr1);
        // The clear ends with both halts cleared, as the spec requires.
        assert_eq!(*be.transport.resets.lock().unwrap(), vec!["in", "out"]);
    }

    #[tokio::test]
    async fn plain_usbtmc_init_skips_ren_and_refuses_488_operations() {
        let mut be = ready(plain_caps()).await;
        assert!(!be.transport.requests().contains(&REQ_REN_CONTROL));
        for (name, r) in [
            ("ren", be.ren(true).await),
            ("gtl", be.go_to_local(0).await),
            ("llo", be.local_lockout().await),
            ("trigger", be.trigger(0).await),
            ("spoll", be.serial_poll(0).await.map(|_| ())),
        ] {
            let e = r.unwrap_err().to_string();
            assert!(e.contains("not USB488"), "{name}: {e}");
        }
        assert!(be.transport.written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn writes_are_framed_with_rising_tags_and_eom() {
        let mut be = ready(usb488_caps()).await;
        be.write(5, b"*RST", true).await.unwrap();
        be.write(5, b"*CLS", false).await.unwrap();
        let w = be.transport.written.lock().unwrap().clone();
        assert_eq!(w[0], encode_dev_dep_msg_out(1, b"*RST", true));
        assert_eq!(w[1], encode_dev_dep_msg_out(2, b"*CLS", false));
        assert!(be.pending_out.is_none());
    }

    #[tokio::test]
    async fn read_requests_then_returns_data_and_end() {
        let mut be = ready(usb488_caps()).await;
        be.transport
            .queue_in(Ok(in_transfer(1, b"+1.234E+00\n", 0x01)));
        let (data, end) = be.read(5, 100).await.unwrap();
        assert_eq!(data, b"+1.234E+00\n");
        assert!(end);
        let req = be.transport.written.lock().unwrap()[0].clone();
        assert_eq!(req, encode_request_dev_dep_msg_in(1, 100, None));
        assert!(be.pending_in.is_none());
    }

    #[tokio::test]
    async fn read_without_eom_reports_more_to_come() {
        let mut be = ready(usb488_caps()).await;
        be.transport.queue_in(Ok(in_transfer(1, b"ABCD", 0x00)));
        let (data, end) = be.read(5, 4).await.unwrap();
        assert_eq!(data, b"ABCD");
        assert!(!end, "no EOM and no TermChar means the message continues");
    }

    #[tokio::test]
    async fn read_uses_term_char_only_when_supported_and_enabled() {
        let mut be = ready(usb488_caps()).await;
        be.set_eos(b'\n', true);
        be.transport.queue_in(Ok(in_transfer(1, b"X\n", 0x02)));
        let (_, end) = be.read(5, 64).await.unwrap();
        assert!(end, "TermChar seen counts as END");
        let req = be.transport.written.lock().unwrap()[0].clone();
        assert_eq!(&req[8..10], &[0x02, b'\n']);

        let mut be = ready(plain_caps()).await;
        be.set_eos(b'\n', true);
        be.transport.queue_in(Ok(in_transfer(1, b"X\n", 0x01)));
        be.read(5, 64).await.unwrap();
        let req = be.transport.written.lock().unwrap()[0].clone();
        assert_eq!(&req[8..10], &[0, 0], "no TermChar on a device without it");
    }

    #[tokio::test]
    async fn read_request_is_capped() {
        let mut be = ready(usb488_caps()).await;
        be.transport.queue_in(Ok(in_transfer(1, b"", 0x01)));
        be.read(5, usize::MAX).await.unwrap();
        let req = be.transport.written.lock().unwrap()[0].clone();
        let size = u32::from_le_bytes([req[4], req[5], req[6], req[7]]);
        assert_eq!(size as usize, MAX_READ_TRANSFER);
    }

    #[tokio::test]
    async fn failed_read_aborts_bulk_in_before_the_next_transfer() {
        let mut be = ready(usb488_caps()).await;
        be.transport.queue_in(Err("timed out"));
        let e = format!("{:#}", be.read(5, 64).await.unwrap_err());
        assert!(e.contains("timed out"), "{e}");
        // Aborted straight away, against the read's own tag and endpoint.
        let c = be.transport.controls.lock().unwrap().clone();
        assert_eq!(
            c[0],
            (Recipient::Endpoint, REQ_INITIATE_ABORT_BULK_IN, 1, 0x82)
        );
        assert_eq!(c[1].1, REQ_CHECK_ABORT_BULK_IN_STATUS);
        assert!(be.pending_in.is_none(), "recovered, nothing left pending");
        assert_eq!(*be.transport.resets.lock().unwrap(), vec!["in", "in"]);

        // And the pipe is usable again without further ceremony.
        be.transport.controls.lock().unwrap().clear();
        be.transport.queue_in(Ok(in_transfer(2, b"ok", 0x01)));
        let (d, _) = be.read(5, 64).await.unwrap();
        assert_eq!(d, b"ok");
        assert!(be.transport.controls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn out_of_step_reply_recovers_by_aborting_bulk_in() {
        let mut be = ready(usb488_caps()).await;
        // The device answers with the reply to some earlier request.
        be.transport.queue_in(Ok(in_transfer(9, b"stale", 0x01)));
        let e = format!("{:#}", be.read(5, 64).await.unwrap_err());
        assert!(e.contains("out of step"), "{e}");
        // Recovery drains the pipe by aborting bulk-IN, which reads out the
        // stale reply. It does not touch bulk-OUT or clear the whole device
        // for what is only a stale read.
        let reqs = be.transport.requests();
        assert!(reqs.contains(&REQ_INITIATE_ABORT_BULK_IN), "{reqs:?}");
        assert!(
            !reqs.contains(&REQ_INITIATE_ABORT_BULK_OUT) && !reqs.contains(&REQ_INITIATE_CLEAR),
            "should not escalate: {reqs:?}"
        );
        assert!(be.pending_out.is_none() && be.pending_in.is_none());
    }

    #[tokio::test]
    async fn abort_escalates_to_clear_when_the_handshake_fails() {
        struct Stubborn(MockTransport);
        #[async_trait::async_trait]
        impl TmcTransport for Stubborn {
            async fn bulk_out(&self, d: &[u8]) -> Result<()> {
                self.0.bulk_out(d).await
            }
            async fn bulk_in(&self, m: usize) -> Result<Vec<u8>> {
                self.0.bulk_in(m).await
            }
            async fn control_in(
                &self,
                r: Recipient,
                req: u8,
                v: u16,
                i: u16,
                l: usize,
            ) -> Result<Vec<u8>> {
                if req == REQ_INITIATE_ABORT_BULK_IN {
                    self.0.controls.lock().unwrap().push((r, req, v, i));
                    return Ok(vec![STATUS_FAILED, v as u8]);
                }
                self.0.control_in(r, req, v, i, l).await
            }
            async fn reset_bulk_in(&self) -> Result<()> {
                self.0.reset_bulk_in().await
            }
            async fn reset_bulk_out(&self) -> Result<()> {
                self.0.reset_bulk_out().await
            }
            fn interface_number(&self) -> u8 {
                1
            }
            fn bulk_in_address(&self) -> u8 {
                0x82
            }
            fn bulk_out_address(&self) -> u8 {
                0x02
            }
            fn set_timeout(&self, _: u32) {}
        }
        let mut be = UsbtmcBackend::new(Stubborn(MockTransport::new(usb488_caps())), 3000);
        be.init(0).await.unwrap();
        be.transport.0.controls.lock().unwrap().clear();
        be.transport.0.queue_in(Err("timed out"));
        assert!(be.read(5, 64).await.is_err());
        let reqs: Vec<u8> = be.transport.0.requests();
        assert_eq!(
            reqs,
            vec![
                REQ_INITIATE_ABORT_BULK_IN,
                REQ_INITIATE_CLEAR,
                REQ_CHECK_CLEAR_STATUS
            ]
        );
        assert!(be.pending_in.is_none());
    }

    #[tokio::test]
    async fn trigger_sends_the_usb488_message_when_advertised() {
        let mut be = ready(usb488_caps()).await;
        be.trigger(5).await.unwrap();
        assert_eq!(be.transport.written.lock().unwrap()[0], encode_trigger(1));

        let mut caps = usb488_caps();
        caps[14] &= !0x01;
        let mut be = ready(caps).await;
        let e = be.trigger(5).await.unwrap_err().to_string();
        assert!(e.contains("TRIGGER"), "{e}");
        assert!(be.transport.written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn serial_poll_takes_the_byte_from_the_reply_without_an_interrupt_endpoint() {
        let mut be = ready(usb488_caps()).await;
        assert_eq!(be.serial_poll(5).await.unwrap(), 0x50);
        let c = be.transport.controls.lock().unwrap()[0];
        assert_eq!(c.1, REQ_READ_STATUS_BYTE);
        assert!((2..=127).contains(&c.2), "status tag {} out of range", c.2);
        // Tags advance so a stale notification can never be mistaken.
        be.serial_poll(5).await.unwrap();
        let c2 = be.transport.controls.lock().unwrap()[1];
        assert_ne!(c.2, c2.2);
    }

    #[tokio::test]
    async fn serial_poll_waits_for_the_interrupt_notification_when_there_is_one() {
        let mut t = MockTransport::new(usb488_caps());
        t.interrupt = true;
        t.stb = 0x44;
        let mut be = UsbtmcBackend::new(t, 3000);
        be.init(0).await.unwrap();
        assert_eq!(be.serial_poll(5).await.unwrap(), 0x44);
        assert!(be.transport.notified.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn serial_poll_falls_back_to_the_control_reply_when_the_interrupt_is_silent() {
        // A Rigol DHO800: interrupt endpoint present, notification never
        // delivered, status byte in the control reply instead.
        let mut t = MockTransport::new(usb488_caps());
        t.interrupt = true;
        t.deliver_notification = false;
        t.stb = 0x41;
        let mut be = UsbtmcBackend::new(t, 3000);
        be.init(0).await.unwrap();
        assert_eq!(be.serial_poll(5).await.unwrap(), 0x41);
    }

    #[tokio::test]
    async fn remote_local_requests_go_to_the_interface() {
        let mut be = ready(usb488_caps()).await;
        be.go_to_remote(5).await.unwrap();
        be.go_to_local(5).await.unwrap();
        be.local_lockout().await.unwrap();
        be.ren(false).await.unwrap();
        let c = be.transport.controls.lock().unwrap().clone();
        assert_eq!(c[0], (Recipient::Interface, REQ_REN_CONTROL, 1, 1));
        assert_eq!(c[1], (Recipient::Interface, REQ_GO_TO_LOCAL, 0, 1));
        assert_eq!(c[2], (Recipient::Interface, REQ_LOCAL_LOCKOUT, 0, 1));
        assert_eq!(c[3], (Recipient::Interface, REQ_REN_CONTROL, 0, 1));

        let mut caps = usb488_caps();
        caps[14] &= !0x02;
        let mut be = ready(caps).await;
        assert!(be.ren(true).await.is_err());
        assert!(be.transport.controls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ifc_and_pad_are_harmless() {
        let mut be = ready(usb488_caps()).await;
        be.ifc().await.unwrap();
        assert!(be.transport.controls.lock().unwrap().is_empty());
        assert!(be.transport.written.lock().unwrap().is_empty());
        // Every primary address is the one device.
        be.write(0, b"A", true).await.unwrap();
        be.write(30, b"B", true).await.unwrap();
        assert_eq!(be.transport.written.lock().unwrap().len(), 2);
        assert_eq!(be.controller_pad(), 0);
        be.set_controller_pad(21).await.unwrap();
        assert_eq!(be.controller_pad(), 21);
    }

    #[tokio::test]
    async fn shutdown_drops_ren_only_when_the_device_has_it() {
        let mut be = ready(usb488_caps()).await;
        be.shutdown().await.unwrap();
        let c = be.transport.controls.lock().unwrap()[0];
        assert_eq!(c, (Recipient::Interface, REQ_REN_CONTROL, 0, 1));

        let mut be = ready(plain_caps()).await;
        be.shutdown().await.unwrap();
        assert!(be.transport.controls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refusing_defaults_stay_in_place() {
        let mut be = ready(usb488_caps()).await;
        assert!(be.srq_asserted().await.is_err());
        assert!(be.bus_lines().await.is_err());
        assert!(be.set_listen_only(true).await.is_err());
        assert!(be.set_device_mode(Some(5)).await.is_err());
        assert!(be.send_bus_command(&[0x3f]).await.is_err());
        assert!(be.set_atn(true).await.is_err());
        assert!(
            be.subscribe_srq().is_none(),
            "no interrupt endpoint, no SRQ path"
        );
    }
}
