// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// USB layer for the NI GPIB-USB-HS: discovery, endpoint wiring, bulk/control
// transfers, and the post-enumeration readiness handshake. Translated from
// `drivers/gpib/ni_usb/ni_usb_gpib.c`.
//
// Verified on a physical GPIB-USB-HS — see the module docs for what was tested.

use std::time::Duration;

use anyhow::{Context, Result};
use nusb::transfer::{Buffer, Bulk, ControlIn, ControlType, In, Interrupt, Out, Recipient};
use nusb::{Endpoint, MaybeFuture};
use tracing::{debug, info, warn};

use super::protocol::{IBSTA_DEFINED_BITS, IBSTA_SRQI, NIUSB_TERM_ID};
use super::NiTransport;

pub const USB_VENDOR_ID_NI: u16 = 0x3923;
pub const PID_NI_USB_HS: u16 = 0x709b;
pub const PID_NI_USB_HS_PLUS: u16 = 0x7618;
pub const PID_KUSB_488A: u16 = 0x725c;
pub const PID_MC_USB_488: u16 = 0x725d;

/// How many service-request notifications to buffer. SRQ is a level, not a
/// count: a lagging subscriber only needs to learn that *someone* asked for
/// service, so a small buffer is plenty and lagging is harmless.
const SRQ_CHANNEL_CAPACITY: usize = 16;

/// Cap on vendor control transfers. These are all short status/handshake
/// exchanges, so anything approaching this means the adapter is not responding.
const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

// Vendor control requests.
const NI_USB_STOP_REQUEST: u8 = 0x20;
const NI_USB_WAIT_REQUEST: u8 = 0x21;
const NI_USB_POLL_READY_REQUEST: u8 = 0x40;
const NI_USB_SERIAL_NUMBER_REQUEST: u8 = 0x41;

/// Bulk/interrupt endpoint addresses for a given product. NI hard-codes these
/// per PID rather than discovering them from the interface descriptor.
struct Endpoints {
    bulk_out: u8,
    bulk_in: u8,
    interrupt_in: u8,
}

/// Render a bulk packet as hex for the debug log. These packets are short
/// (tens of bytes) and the framing is only readable byte-by-byte.
fn hex(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn endpoints_for(pid: u16) -> Endpoints {
    match pid {
        // HS+ places GPIB on OUT 0x01 / IN 0x02 (IN 0x02 -> wire 0x82),
        // interrupt on 0x03 (-> wire 0x83).
        PID_NI_USB_HS_PLUS => Endpoints {
            bulk_out: 0x01,
            bulk_in: 0x82,
            interrupt_in: 0x83,
        },
        // HS, KUSB-488A, MC-USB-488: OUT 0x02 / IN 0x04 (IN -> wire 0x84),
        // interrupt on 0x01 (-> wire 0x81).
        _ => Endpoints {
            bulk_out: 0x02,
            bulk_in: 0x84,
            interrupt_in: 0x81,
        },
    }
}

/// Endpoint addresses for `pid` as `(bulk_out, bulk_in, interrupt_in)`.
#[cfg(test)]
pub fn endpoints_for_test(pid: u16) -> (u8, u8, u8) {
    let e = endpoints_for(pid);
    (e.bulk_out, e.bulk_in, e.interrupt_in)
}

/// The bulk endpoints plus a note of whether the last exchange finished.
///
/// Every adapter operation is a bulk-out and the bulk-in carrying its status,
/// and the two are not separable: anything that slips between them leaves the
/// reply to be read by the *next* request, and every reply from then on answers
/// the one before. Holding both endpoints behind a single lock is what keeps
/// the pair indivisible against the interrupt reader's re-arm, which is a
/// control transfer on the same device.
///
/// `abandoned` covers the other way the pair can be broken: a HiSLIP client
/// that gives up cancels the task mid-await, and a failed bulk-in returns
/// early. Either way the adapter still sends the reply. The flag is set before
/// the exchange and cleared only on success, so the next caller knows to drain.
struct BulkIo {
    out: Endpoint<Bulk, Out>,
    r#in: Endpoint<Bulk, In>,
    abandoned: bool,
}

pub struct NiUsbTransport {
    interface: nusb::Interface,
    device: nusb::Device,
    io: tokio::sync::Mutex<BulkIo>,
    pid: u16,
    timeout_ms: u32,
    srq: tokio::sync::broadcast::Sender<()>,
    /// Aborted on drop, which is what stops the adapter reporting to nobody.
    _reader_task: tokio::task::JoinHandle<()>,
}

impl NiUsbTransport {
    /// Find the first connected NI GPIB-USB-HS-compatible adapter, claim its
    /// GPIB interface, and wire up the fixed endpoints.
    pub async fn open(timeout_ms: u32, port: Option<&str>) -> Result<Self> {
        let (dev_info, pid) = find_device(port)?;
        let device = dev_info
            .open()
            .wait()
            .context("failed to open NI USB device")?;
        let interface = device.claim_interface(0).wait().context(
            "failed to claim NI GPIB interface 0 — is the kernel ni_usb driver loaded? \
             Blacklist it (see README) to use the userspace driver",
        )?;
        let eps = endpoints_for(pid);
        info!(
            "NI adapter open (PID {pid:#06x}), bulk out {:#04x} in {:#04x}",
            eps.bulk_out, eps.bulk_in
        );

        let io = tokio::sync::Mutex::new(BulkIo {
            out: interface
                .endpoint::<Bulk, Out>(eps.bulk_out)
                .with_context(|| format!("open bulk-out endpoint {:#04x}", eps.bulk_out))?,
            r#in: interface
                .endpoint::<Bulk, In>(eps.bulk_in)
                .with_context(|| format!("open bulk-in endpoint {:#04x}", eps.bulk_in))?,
            abandoned: false,
        });

        // Read the interrupt endpoint from the moment the adapter is open, and
        // before anything arms monitoring. A reported bit nobody reads backs up
        // and stalls the adapter's bulk transfers on the *next* session,
        // recoverable only by replugging, so the reader is not an optional
        // companion to SRQ support — it is what makes arming safe at all.
        let (srq, _) = tokio::sync::broadcast::channel(SRQ_CHANNEL_CAPACITY);
        let irq_ep = interface
            .endpoint::<Interrupt, In>(eps.interrupt_in)
            .with_context(|| format!("open interrupt endpoint {:#04x}", eps.interrupt_in))?;
        let reader_srq = srq.clone();
        let reader_task = tokio::spawn(interrupt_reader(irq_ep, reader_srq));

        Ok(Self {
            interface,
            device,
            io,
            pid,
            timeout_ms,
            srq,
            _reader_task: reader_task,
        })
    }

    /// Receiver for service-request notifications from the interrupt endpoint.
    pub fn subscribe_srq(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.srq.subscribe()
    }

    fn bulk_timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms as u64 + 2000)
    }

    /// Abort any transfer the adapter still believes is in flight and clear
    /// stalled endpoints.
    ///
    /// This matters on every open, not just after an error: the adapter keeps
    /// its GPIB state across host process restarts, so a daemon killed
    /// mid-transfer leaves a pending operation that makes the next session's
    /// first bulk-in block until it times out.
    pub async fn quiesce(&self) {
        match NiTransport::control_in(self, NI_USB_STOP_REQUEST, 0, 0, 8).await {
            Ok(reply) => debug!(?reply, "ni stop request"),
            Err(e) => debug!("ni stop request failed (adapter may be idle): {e}"),
        }
        {
            let mut io = self.io.lock().await;
            let _ = MaybeFuture::wait(io.out.clear_halt());
            let _ = MaybeFuture::wait(io.r#in.clear_halt());
        }
        self.drain_bulk_in().await;
    }

    /// Read and discard any bulk response left queued by a previous session.
    ///
    /// Every adapter operation is a bulk-out followed by a bulk-in carrying its
    /// status. If a process dies (or times out) between the two, that response
    /// stays queued, and the next session reads it in place of its own — every
    /// reply then answers the *previous* request. The visible symptoms are
    /// wrong-sized status blocks and operations that appear to hang because
    /// their response was already consumed by the one before.
    pub async fn drain_bulk_in(&self) {
        let mut io = self.io.lock().await;
        drain_bulk_in_locked(&mut io).await;
    }
}

/// Submit an OUT transfer and wait for it. Caller holds the I/O lock.
async fn bulk_out_locked(io: &mut BulkIo, data: &[u8], timeout: Duration) -> Result<()> {
    debug!(len = data.len(), packet = %hex(data), "ni bulk-out");
    io.out.submit(data.to_vec().into());
    let completion = tokio::time::timeout(timeout, io.out.next_complete())
        .await
        .context("ni bulk-out timed out")?;
    completion
        .status
        .map_err(|e| anyhow::anyhow!("ni bulk-out failed: {e}"))?;
    Ok(())
}

/// Submit an IN transfer and wait for it. Caller holds the I/O lock.
async fn bulk_in_locked(io: &mut BulkIo, max_len: usize, timeout: Duration) -> Result<Vec<u8>> {
    /// Cap on a single IN request, a whole number of any plausible packet size.
    const MAX_REQUEST: usize = 16 * 1024;
    const TERMINATION: [u8; 4] = [NIUSB_TERM_ID, 0x00, 0x00, 0x00];

    // An IN transfer must request a whole number of max-size packets.
    let mps = io.r#in.max_packet_size().max(1);
    let mut data: Vec<u8> = Vec::with_capacity(max_len.min(4096));

    // A long reply arrives as several transfers, each ending at a packet
    // boundary, so one completion yields only the first packet and the caller
    // sees a truncated status block. Packet boundaries cannot say where the
    // reply ends, but the framing can: every reply finishes with a termination
    // block. Read until that arrives.
    //
    // `timeout` bounds each individual read and `max_len` the total, so a reply
    // that never terminates cannot hang here.
    while data.len() < max_len && !data.ends_with(&TERMINATION) {
        let want = (max_len - data.len()).clamp(1, MAX_REQUEST).div_ceil(mps) * mps;
        io.r#in.submit(Buffer::new(want));
        let completion = tokio::time::timeout(timeout, io.r#in.next_complete())
            .await
            .context("ni bulk-in timed out")?;
        completion
            .status
            .map_err(|e| anyhow::anyhow!("ni bulk-in failed: {e}"))?;
        debug!(
            got = completion.buffer.len(),
            want,
            have = data.len(),
            "ni bulk-in chunk"
        );
        if completion.buffer.is_empty() {
            // A zero-length packet ends the transfer; reading on would spin.
            break;
        }
        data.extend_from_slice(&completion.buffer);
    }

    debug!(len = data.len(), packet = %hex(&data), "ni bulk-in");
    Ok(data)
}

/// Issue a vendor control-IN against the device. Caller holds the I/O lock.
async fn vendor_control_in(
    device: &nusb::Device,
    request: u8,
    value: u16,
    index: u16,
    max_len: usize,
) -> Result<Vec<u8>> {
    device
        .control_in(
            ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request,
                value,
                index,
                length: max_len as u16,
            },
            CONTROL_TIMEOUT,
        )
        .await
        .map_err(|e| anyhow::anyhow!("ni control-in failed: {e}"))
}

/// Read and discard any bulk response left queued by an earlier exchange.
///
/// Caller holds the I/O lock.
async fn drain_bulk_in_locked(io: &mut BulkIo) {
    let mps = io.r#in.max_packet_size().max(1);
    for _ in 0..16 {
        io.r#in.submit(Buffer::new(256usize.div_ceil(mps) * mps));
        match tokio::time::timeout(Duration::from_millis(100), io.r#in.next_complete()).await {
            Ok(c) if c.status.is_ok() && !c.buffer.is_empty() => {
                debug!(packet = %hex(&c.buffer), "ni drained stale bulk response");
            }
            // Timed out or errored: the endpoint is empty, which is normal.
            // next_complete is cancel-safe, so the timed-out transfer stays
            // pending and is picked up by the next read rather than lost.
            _ => return,
        }
    }
    debug!("ni bulk drain hit its cap; leaving the rest");
}

/// Read the interrupt endpoint for as long as the adapter is open, publishing
/// service requests.
///
/// The adapter only reports when monitoring is armed, and the monitor is
/// one-shot *per bit*: the kernel driver clears each reported bit from the set
/// it waits on (`monitored_ibsta_bits &= ~status.ibsta`), so a notification
/// consumes the arming that produced it. Re-arming is the backend's job — it is
/// a control transfer and has to be serialised against bulk traffic, which only
/// the transport's I/O lock can do.
///
/// Errors are treated as "keep going": a transient failure must not quietly end
/// the reader and leave the adapter reporting to nobody.
async fn interrupt_reader(
    mut endpoint: Endpoint<Interrupt, In>,
    srq: tokio::sync::broadcast::Sender<()>,
) {
    let buf_len = {
        let mps = endpoint.max_packet_size().max(1);
        64usize.div_ceil(mps) * mps
    };
    loop {
        endpoint.submit(Buffer::new(buf_len));
        let completion = endpoint.next_complete().await;
        match completion.status {
            Ok(()) if completion.buffer.is_empty() => {}
            Ok(()) => {
                let ibsta = parse_interrupt_ibsta(&completion.buffer);
                let asserted = ibsta.is_some_and(|v| v & IBSTA_SRQI != 0);
                debug!(
                    packet = %hex(&completion.buffer),
                    ibsta = ?ibsta.map(|v| format!("{v:#06x}")),
                    asserted,
                    "ni interrupt report"
                );
                if asserted {
                    // No subscribers is normal; a send error is not a problem.
                    let _ = srq.send(());
                }
            }
            Err(e) => {
                debug!("ni interrupt read failed, continuing: {e}");
                let _ = MaybeFuture::wait(endpoint.clear_halt());
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Pull the ibsta word out of an interrupt report.
///
/// Same layout as any status block, per `ni_usb_parse_status_block`: an id byte
/// then ibsta big-endian. Reports too short, or carrying bits that are not
/// defined ibsta bits, are rejected rather than guessed at — arming is
/// acknowledged on this endpoint with a block that parses as `0xff00`, which
/// has SRQI set and would otherwise read as a request nobody made.
fn parse_interrupt_ibsta(packet: &[u8]) -> Option<u16> {
    if packet.len() < 3 {
        return None;
    }
    let ibsta = (u16::from(packet[1]) << 8) | u16::from(packet[2]);
    if ibsta & !IBSTA_DEFINED_BITS != 0 {
        return None;
    }
    Some(ibsta)
}

/// Re-apply the device's current USB configuration, mirroring the kernel
/// driver's `usb_reset_configuration()` at attach. This resets endpoint state
/// and data toggles without re-enumerating, which can clear a stale operation
/// left behind by a previous session.
///
/// NOTE: deliberately *not* `Device::reset()`. That looks like the obvious way
/// to recover a wedged adapter, but on macOS this hardware does not come back
/// from it — the device leaves the bus and stays gone until physically
/// replugged, turning a recoverable wedge into a dead adapter.
pub fn reset_configuration(port: Option<&str>) -> Result<()> {
    let (dev_info, _pid) = find_device(port)?;
    let device = dev_info
        .open()
        .wait()
        .context("failed to reopen NI USB device")?;
    let config = device
        .active_configuration()
        .map(|c| c.configuration_value())
        .unwrap_or(1);
    device
        .set_configuration(config)
        .wait()
        .with_context(|| format!("failed to re-apply USB configuration {config}"))?;
    debug!("re-applied USB configuration {config}");
    Ok(())
}

/// Locate an NI GPIB-USB-HS-compatible adapter, returning its info and PID,
/// restricted to `port` (USB port id) when given.
fn find_device(port: Option<&str>) -> Result<(nusb::DeviceInfo, u16)> {
    for dev in nusb::list_devices()
        .wait()
        .context("failed to list USB devices")?
    {
        if dev.vendor_id() != USB_VENDOR_ID_NI {
            continue;
        }
        let pid = dev.product_id();
        if !super::USB_IDS.contains(&(USB_VENDOR_ID_NI, pid)) {
            continue;
        }
        if let Some(want) = port {
            if crate::backend::select::port_id(&dev) != want {
                continue;
            }
        }
        return Ok((dev, pid));
    }
    match port {
        Some(want) => anyhow::bail!("no NI GPIB-USB-HS adapter found at USB port {want:?}"),
        None => anyhow::bail!(
            "no NI GPIB-USB-HS adapter found (expected VID {:#06x})",
            USB_VENDOR_ID_NI
        ),
    }
}

#[async_trait::async_trait]
impl NiTransport for NiUsbTransport {
    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        Some(NiUsbTransport::subscribe_srq(self))
    }

    async fn bulk_out(&self, data: &[u8]) -> Result<()> {
        let mut io = self.io.lock().await;
        bulk_out_locked(&mut io, data, self.bulk_timeout()).await
    }

    async fn bulk_in(&self, max_len: usize) -> Result<Vec<u8>> {
        let mut io = self.io.lock().await;
        bulk_in_locked(&mut io, max_len, self.bulk_timeout()).await
    }

    async fn transact(&self, req: &[u8], resp_len: usize) -> Result<Vec<u8>> {
        let timeout = self.bulk_timeout();
        let mut io = self.io.lock().await;

        if io.abandoned {
            debug!("ni: draining after an abandoned transaction");
            drain_bulk_in_locked(&mut io).await;
            io.abandoned = false;
        }

        // Assume the worst until the pair completes. If this future is dropped
        // between the halves, or the bulk-in fails, the flag stays set and the
        // next transaction clears the orphaned reply before using the endpoint.
        io.abandoned = true;
        bulk_out_locked(&mut io, req, timeout).await?;
        let resp = bulk_in_locked(&mut io, resp_len, timeout).await?;
        io.abandoned = false;
        Ok(resp)
    }

    /// Re-arm the adapter's interrupt monitor.
    ///
    /// Takes the I/O lock: this is a control transfer, and one landing between
    /// a bulk-out and its bulk-in desynchronises every reply that follows.
    async fn rearm_srq(&self, mask: u16) -> Result<()> {
        let _io = self.io.lock().await;
        vendor_control_in(&self.device, NI_USB_WAIT_REQUEST, 0x300, mask, 8).await?;
        Ok(())
    }

    async fn control_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        max_len: usize,
    ) -> Result<Vec<u8>> {
        // Bound the wait: a wedged adapter accepts the request and never
        // completes it, which would otherwise hang the daemon forever.
        let _io = self.io.lock().await;
        vendor_control_in(&self.device, request, value, index, max_len).await
    }

    async fn control_in_interface(
        &self,
        request: u8,
        value: u16,
        index: u16,
        max_len: usize,
    ) -> Result<Vec<u8>> {
        let _io = self.io.lock().await;
        self.interface
            .control_in(
                ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Interface,
                    request,
                    value,
                    index,
                    length: max_len as u16,
                },
                CONTROL_TIMEOUT,
            )
            .await
            .map_err(|e| anyhow::anyhow!("ni interface control-in failed: {e}"))
    }

    fn product_id(&self) -> u16 {
        self.pid
    }
}

/// Extra bring-up the HS+ needs after the readiness handshake, mirroring
/// `ni_usb_hs_plus_extra_init`. Three vendor reads whose replies the kernel only
/// sanity-checks, so we log mismatches rather than failing.
///
/// Verified on a GPIB-USB-HS+ (serial 01A87F99). The LED and interface-recipient
/// replies come back exactly as the kernel driver expects; the first request
/// answers `48 cd 75 00 ...` where the driver's example shows `48 f3 30 00 ...`,
/// so only byte 0 — the echoed request id — is worth checking, which is all
/// either driver does.
pub async fn hs_plus_extra_init<T: NiTransport>(transport: &T) -> Result<()> {
    // Expected: 48 f3 30 00 00 ...
    let reply = transport.control_in(0x48, 0x0, 0x0, 16).await?;
    check_reply("HS+ 0x48", 0x48, &reply);

    // LED request. Expected: 4b 00
    let reply = transport.control_in(0x4b, 0x1, 0x0, 2).await?;
    check_reply("HS+ LED", 0x4b, &reply);

    // Interface-recipient request. Expected: f8 01 00 00 00 01 00 00 00
    let reply = transport.control_in_interface(0xf8, 0x0, 0x1, 9).await?;
    check_reply("HS+ 0xf8", 0xf8, &reply);

    info!("NI GPIB-USB-HS+ extra init complete");
    Ok(())
}

/// The HS+ init replies echo their request id in byte 0; anything else is worth
/// seeing in the log but is not fatal.
fn check_reply(what: &str, expected_id: u8, reply: &[u8]) {
    match reply.first() {
        Some(&id) if id == expected_id => debug!(packet = %hex(reply), "ni {what} reply"),
        _ => warn!(packet = %hex(reply), "ni {what}: unexpected reply id"),
    }
}

/// Tell the adapter which ibsta bits to watch, mirroring
/// `ni_usb_set_interrupt_monitor`. The kernel issues this once with an empty
/// mask before the init register sequence and again with the full mask after,
/// and the adapter expects both before it will service addressed transfers.
pub async fn set_interrupt_monitor<T: NiTransport>(
    transport: &T,
    monitored_bits: u16,
) -> Result<()> {
    let reply = transport
        .control_in(NI_USB_WAIT_REQUEST, 0x300, monitored_bits, 8)
        .await
        .context("ni interrupt-monitor request failed")?;
    if reply.len() != 8 {
        anyhow::bail!(
            "ni interrupt-monitor: expected an 8-byte status block, got {}",
            reply.len()
        );
    }
    debug!(mask = monitored_bits, ?reply, "ni set interrupt monitor");
    Ok(())
}

/// Post-enumeration readiness handshake, mirroring `ni_usb_hs_wait_for_ready`.
/// The driver reads the serial number, then polls a 16-byte status block until
/// the adapter reports ready.
///
/// Readiness is signalled by any of bytes 6, 7 or 10 becoming non-zero; their
/// exact values vary by model (NI-USB-HS, HS+, MC-USB-488, KUSB-488A) and are
/// informational only, so we key on non-zero rather than on a value whitelist.
pub async fn wait_for_ready<T: NiTransport>(transport: &T) -> Result<()> {
    // Serial-number request. The reply echoes the request id in byte 0 followed
    // by a little-endian 32-bit serial (5 bytes; the HS+ zero-pads to 16).
    match transport
        .control_in(NI_USB_SERIAL_NUMBER_REQUEST, 0, 0, 16)
        .await
    {
        Ok(reply) if reply.len() >= 5 => {
            if reply[0] != NI_USB_SERIAL_NUMBER_REQUEST {
                debug!(?reply, "ni serial-number reply had unexpected id");
            }
            let serial = u32::from_le_bytes([reply[1], reply[2], reply[3], reply[4]]);
            debug!("ni board serial number {serial:#x}");
        }
        Ok(reply) => debug!(len = reply.len(), "ni serial-number reply short"),
        Err(e) => debug!("ni serial-number request failed: {e}"),
    }

    // A device still booting answers but reports not-ready; one whose handle is
    // broken fails the transfer outright. Only the former is worth waiting out,
    // so give up quickly on a run of transport errors and let the caller reset.
    const MAX_CONSECUTIVE_ERRORS: u32 = 5;
    let mut consecutive_errors = 0u32;

    for attempt in 0..50 {
        match transport
            .control_in(NI_USB_POLL_READY_REQUEST, 0, 0, 16)
            .await
        {
            Ok(status) if status.len() >= 11 => {
                consecutive_errors = 0;
                debug!(?status, attempt, "ni poll-ready");
                if status[6] != 0 || status[7] != 0 || status[10] != 0 {
                    info!("NI adapter ready after {attempt} poll(s)");
                    return Ok(());
                }
            }
            Ok(status) => {
                consecutive_errors = 0;
                debug!(len = status.len(), attempt, "ni poll-ready reply short");
            }
            Err(e) => {
                consecutive_errors += 1;
                debug!("ni poll-ready attempt {attempt} error: {e}");
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    anyhow::bail!(
                        "NI adapter is not answering control requests \
                         ({consecutive_errors} consecutive failures); last error: {e}"
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("NI adapter did not report ready after 5s of polling")
}
