// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// USB layer for the NI GPIB-USB-HS: discovery, endpoint wiring, bulk/control
// transfers, and the post-enumeration readiness handshake. Translated from
// `drivers/gpib/ni_usb/ni_usb_gpib.c`.
//
// EXPERIMENTAL / untested on hardware — see the module docs.

use std::time::Duration;

use anyhow::{Context, Result};
use nusb::transfer::{ControlIn, ControlType, Recipient, RequestBuffer};
use tracing::{debug, info};

use super::NiTransport;

pub const USB_VENDOR_ID_NI: u16 = 0x3923;
pub const PID_NI_USB_HS: u16 = 0x709b;
pub const PID_NI_USB_HS_PLUS: u16 = 0x7618;
pub const PID_KUSB_488A: u16 = 0x725c;
pub const PID_MC_USB_488: u16 = 0x725d;

// Vendor control requests.
const NI_USB_POLL_READY_REQUEST: u8 = 0x40;
const NI_USB_SERIAL_NUMBER_REQUEST: u8 = 0x41;

/// Bulk/interrupt endpoint addresses for a given product. NI hard-codes these
/// per PID rather than discovering them from the interface descriptor.
struct Endpoints {
    bulk_out: u8,
    bulk_in: u8,
}

fn endpoints_for(pid: u16) -> Endpoints {
    match pid {
        // HS+ places GPIB on OUT 0x01 / IN 0x02 (IN 0x02 -> wire 0x82).
        PID_NI_USB_HS_PLUS => Endpoints {
            bulk_out: 0x01,
            bulk_in: 0x82,
        },
        // HS, KUSB-488A, MC-USB-488: OUT 0x02 / IN 0x04 (IN -> wire 0x84).
        _ => Endpoints {
            bulk_out: 0x02,
            bulk_in: 0x84,
        },
    }
}

pub struct NiUsbTransport {
    interface: nusb::Interface,
    device: nusb::Device,
    bulk_out_ep: u8,
    bulk_in_ep: u8,
    timeout_ms: u32,
}

impl NiUsbTransport {
    /// Find the first connected NI GPIB-USB-HS-compatible adapter, claim its
    /// GPIB interface, and wire up the fixed endpoints.
    pub async fn open(timeout_ms: u32) -> Result<Self> {
        let (dev_info, pid) = find_device()?;
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
            timeout_ms,
        })
    }

    fn bulk_timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms as u64 + 2000)
    }
}

/// Locate an NI GPIB-USB-HS-compatible adapter, returning its info and PID.
fn find_device() -> Result<(nusb::DeviceInfo, u16)> {
    for dev in nusb::list_devices().context("failed to list USB devices")? {
        if dev.vendor_id() != USB_VENDOR_ID_NI {
            continue;
        }
        let pid = dev.product_id();
        if super::USB_IDS.contains(&(USB_VENDOR_ID_NI, pid)) {
            return Ok((dev, pid));
        }
    }
    anyhow::bail!(
        "no NI GPIB-USB-HS adapter found (expected VID {:#06x})",
        USB_VENDOR_ID_NI
    )
}

#[async_trait::async_trait]
impl NiTransport for NiUsbTransport {
    async fn bulk_out(&self, data: &[u8]) -> Result<()> {
        debug!(len = data.len(), first = ?&data[..data.len().min(8)], "ni bulk-out");
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
            first = ?&completion.data[..completion.data.len().min(8)],
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
        let completion = self
            .device
            .control_in(ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request,
                value,
                index,
                length: max_len as u16,
            })
            .await;
        completion
            .into_result()
            .map_err(|e| anyhow::anyhow!("ni control-in failed: {e}"))
    }
}

/// Post-enumeration readiness handshake. The kernel driver reads the serial
/// number, then polls a status block until a model-specific "ready" pattern
/// appears.
///
/// TRANSLATED, UNVERIFIED: the exact ready-byte pattern was not captured during
/// porting, so this issues the same control requests to give the adapter time
/// to settle and proceeds best-effort rather than asserting a specific reply.
pub async fn wait_for_ready<T: NiTransport>(transport: &T) -> Result<()> {
    // Serial-number request (the driver does this first).
    let _ = transport
        .control_in(NI_USB_SERIAL_NUMBER_REQUEST, 0, 0, 16)
        .await;
    for attempt in 0..50 {
        match transport
            .control_in(NI_USB_POLL_READY_REQUEST, 0, 0, 16)
            .await
        {
            Ok(status) if !status.is_empty() => {
                debug!(?status, attempt, "ni poll-ready");
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => debug!("ni poll-ready attempt {attempt} error: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("NI adapter did not report ready")
}
