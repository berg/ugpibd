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
use nusb::transfer::{ControlIn, ControlType, Recipient, RequestBuffer};
use tracing::{debug, info, warn};

use super::NiTransport;

pub const USB_VENDOR_ID_NI: u16 = 0x3923;
pub const PID_NI_USB_HS: u16 = 0x709b;
pub const PID_NI_USB_HS_PLUS: u16 = 0x7618;
pub const PID_KUSB_488A: u16 = 0x725c;
pub const PID_MC_USB_488: u16 = 0x725d;

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

pub struct NiUsbTransport {
    interface: nusb::Interface,
    device: nusb::Device,
    bulk_out_ep: u8,
    bulk_in_ep: u8,
    interrupt_in_ep: u8,
    pid: u16,
    timeout_ms: u32,
}

impl NiUsbTransport {
    /// Find the first connected NI GPIB-USB-HS-compatible adapter, claim its
    /// GPIB interface, and wire up the fixed endpoints.
    pub async fn open(timeout_ms: u32, port: Option<&str>) -> Result<Self> {
        let (dev_info, pid) = find_device(port)?;
        let device = dev_info.open().context("failed to open NI USB device")?;
        let interface = device.claim_interface(0).context(
            "failed to claim NI GPIB interface 0 — is the kernel ni_usb driver loaded? \
             Blacklist it (see README) to use the userspace driver",
        )?;
        let eps = endpoints_for(pid);
        info!(
            "NI adapter open (PID {pid:#06x}), bulk out {:#04x} in {:#04x}",
            eps.bulk_out, eps.bulk_in
        );
        Ok(Self {
            interface,
            device,
            bulk_out_ep: eps.bulk_out,
            bulk_in_ep: eps.bulk_in,
            interrupt_in_ep: eps.interrupt_in,
            pid,
            timeout_ms,
        })
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
        for ep in [self.bulk_out_ep, self.bulk_in_ep] {
            if let Err(e) = self.interface.clear_halt(ep) {
                debug!("ni clear_halt on endpoint {ep:#04x} failed: {e}");
            }
        }
        self.drain_bulk_in().await;
        self.drain_interrupts().await;
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
        for _ in 0..16 {
            let mut queue = self.interface.bulk_in_queue(self.bulk_in_ep);
            queue.submit(RequestBuffer::new(256));
            match tokio::time::timeout(Duration::from_millis(100), queue.next_complete()).await {
                Ok(completion) if completion.status.is_ok() && !completion.data.is_empty() => {
                    debug!(packet = %hex(&completion.data), "ni drained stale bulk response");
                }
                // Timed out or errored: the endpoint is empty, which is normal.
                _ => return,
            }
        }
        debug!("ni bulk drain hit its cap; leaving the rest");
    }

    /// Read and discard anything queued on the interrupt endpoint.
    ///
    /// The adapter reports ibsta changes there whenever interrupt monitoring is
    /// enabled. Nothing in this daemon consumes those reports, so a backlog left
    /// by an earlier session can stall the adapter's bulk transfers — the
    /// symptom is the first IFC after a restart never completing.
    pub async fn drain_interrupts(&self) {
        for _ in 0..16 {
            let mut queue = self.interface.interrupt_in_queue(self.interrupt_in_ep);
            queue.submit(RequestBuffer::new(64));
            match tokio::time::timeout(Duration::from_millis(50), queue.next_complete()).await {
                Ok(completion) if completion.status.is_ok() && !completion.data.is_empty() => {
                    debug!(packet = %hex(&completion.data), "ni drained stale interrupt");
                }
                // Timed out or errored: nothing more is queued.
                _ => return,
            }
        }
        debug!("ni interrupt drain hit its cap; leaving the rest");
    }
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
    let device = dev_info.open().context("failed to reopen NI USB device")?;
    let config = device
        .active_configuration()
        .map(|c| c.configuration_value())
        .unwrap_or(1);
    device
        .set_configuration(config)
        .with_context(|| format!("failed to re-apply USB configuration {config}"))?;
    debug!("re-applied USB configuration {config}");
    Ok(())
}

/// Locate an NI GPIB-USB-HS-compatible adapter, returning its info and PID,
/// restricted to `port` (USB port id) when given.
fn find_device(port: Option<&str>) -> Result<(nusb::DeviceInfo, u16)> {
    for dev in nusb::list_devices().context("failed to list USB devices")? {
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
    async fn bulk_out(&self, data: &[u8]) -> Result<()> {
        debug!(len = data.len(), packet = %hex(data), "ni bulk-out");
        let mut queue = self.interface.bulk_out_queue(self.bulk_out_ep);
        queue.submit(data.to_vec());
        let completion = tokio::time::timeout(self.bulk_timeout(), queue.next_complete())
            .await
            .context("ni bulk-out timed out")?;
        completion
            .status
            .map_err(|e| anyhow::anyhow!("ni bulk-out failed: {e}"))?;
        Ok(())
    }

    async fn bulk_in(&self, max_len: usize) -> Result<Vec<u8>> {
        let mut queue = self.interface.bulk_in_queue(self.bulk_in_ep);
        queue.submit(RequestBuffer::new(max_len));
        let completion = tokio::time::timeout(self.bulk_timeout(), queue.next_complete())
            .await
            .context("ni bulk-in timed out")?;
        completion
            .status
            .map_err(|e| anyhow::anyhow!("ni bulk-in failed: {e}"))?;
        debug!(
            len = completion.data.len(),
            packet = %hex(&completion.data),
            "ni bulk-in"
        );
        Ok(completion.data)
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
        let completion = tokio::time::timeout(
            CONTROL_TIMEOUT,
            self.device.control_in(ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request,
                value,
                index,
                length: max_len as u16,
            }),
        )
        .await
        .with_context(|| format!("ni control-in (request {request:#04x}) timed out"))?;
        completion
            .into_result()
            .map_err(|e| anyhow::anyhow!("ni control-in failed: {e}"))
    }

    async fn control_in_interface(
        &self,
        request: u8,
        value: u16,
        index: u16,
        max_len: usize,
    ) -> Result<Vec<u8>> {
        let completion = tokio::time::timeout(
            CONTROL_TIMEOUT,
            self.interface.control_in(ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Interface,
                request,
                value,
                index,
                length: max_len as u16,
            }),
        )
        .await
        .with_context(|| format!("ni interface control-in (request {request:#04x}) timed out"))?;
        completion
            .into_result()
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
/// UNTESTED: no HS+ hardware was available: this is a direct translation.
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
pub async fn set_interrupt_monitor<T: NiTransport>(transport: &T, monitored_bits: u16) -> Result<()> {
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
