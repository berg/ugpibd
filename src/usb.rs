// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors

use std::sync::Arc;

use anyhow::{Context, Result};
use nusb::transfer::{ControlIn, ControlType, Recipient, RequestBuffer};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::gpib::Transport;
use crate::protocol::*;

pub struct UsbTransport {
    interface: nusb::Interface,
    device: nusb::Device,
    bulk_out_ep: u8,
    bulk_in_ep: u8,
    write_complete: Arc<Notify>,
    timeout_ms: u32,
    _irq_task: tokio::task::JoinHandle<()>,
}

impl UsbTransport {
    pub fn new(
        device: nusb::Device,
        interface: nusb::Interface,
        bulk_out_ep: u8,
        bulk_in_ep: u8,
        irq_in_ep: u8,
        timeout_ms: u32,
    ) -> Self {
        let write_complete = Arc::new(Notify::new());
        let notify_clone = write_complete.clone();
        let irq_iface = interface.clone();

        let irq_task = tokio::spawn(async move {
            interrupt_poller(irq_iface, irq_in_ep, notify_clone).await;
        });

        Self {
            interface,
            device,
            bulk_out_ep,
            bulk_in_ep,
            write_complete,
            timeout_ms,
            _irq_task: irq_task,
        }
    }
}

async fn interrupt_poller(
    interface: nusb::Interface,
    endpoint: u8,
    write_complete: Arc<Notify>,
) {
    let mut queue = interface.interrupt_in_queue(endpoint);
    loop {
        queue.submit(RequestBuffer::new(INTERRUPT_BUF_LEN));
        let completion = queue.next_complete().await;
        match completion.status {
            Ok(_) => {
                let flags = completion.data.first().copied().unwrap_or(0);
                debug!(flags = ?format_args!("{:#04x}", flags), "interrupt");
                if flags & (1 << AIF_WRITE_COMPLETE_BN) != 0 {
                    write_complete.notify_one();
                }
            }
            Err(e) => {
                warn!("interrupt endpoint error: {e} — stopping poller");
                break;
            }
        }
    }
}

impl Transport for UsbTransport {
    async fn write_bulk(&self, data: &[u8]) -> Result<()> {
        let mut queue = self.interface.bulk_out_queue(self.bulk_out_ep);
        queue.submit(data.to_vec());
        let completion = queue.next_complete().await;
        completion.status.map_err(|e| anyhow::anyhow!("bulk-out failed: {e}"))?;
        Ok(())
    }

    async fn read_bulk(&self, max_len: usize) -> Result<Vec<u8>> {
        let mut queue = self.interface.bulk_in_queue(self.bulk_in_ep);
        queue.submit(RequestBuffer::new(max_len));
        let completion = queue.next_complete().await;
        completion
            .status
            .map_err(|e| anyhow::anyhow!("bulk-in failed: {e}"))?;
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
        let data = completion
            .into_result()
            .map_err(|e| anyhow::anyhow!("control-in failed: {e}"))?;
        Ok(data)
    }

    async fn await_write_complete(&self) -> Result<()> {
        tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms as u64 + 1000),
            self.write_complete.notified(),
        )
        .await
        .context("timeout waiting for write-complete interrupt")?;
        Ok(())
    }
}

/// Find an 82357B device in either pre-firmware or post-firmware state.
pub fn find_device() -> Result<(nusb::DeviceInfo, u16)> {
    let devices = nusb::list_devices().context("failed to list USB devices")?;
    for dev in devices {
        if dev.vendor_id() != USB_VID_AGILENT {
            continue;
        }
        let pid = dev.product_id();
        if pid == USB_PID_82357B || pid == USB_PID_82357B_PREINIT {
            return Ok((dev, pid));
        }
    }
    anyhow::bail!(
        "no Agilent/Keysight 82357B found (expected VID 0x0957, PID 0x0518 or 0x0718)"
    )
}

/// Open device, claim interface 0, return UsbTransport wired to the 82357B endpoints.
pub async fn open_transport(dev_info: nusb::DeviceInfo, timeout_ms: u32) -> Result<UsbTransport> {
    let device = dev_info.open().context("failed to open USB device")?;
    let interface = device.claim_interface(0).context(
        "failed to claim interface 0 — is the kernel driver loaded? \
         See blacklist instructions in README.md",
    )?;
    Ok(UsbTransport::new(
        device,
        interface,
        EP_82357B_BULK_OUT,
        EP_BULK_IN,
        EP_82357B_IRQ_IN,
        timeout_ms,
    ))
}

/// Poll for a device with the given PID to appear, up to `timeout`.
pub async fn wait_for_pid(
    pid: u16,
    timeout: std::time::Duration,
) -> Result<nusb::DeviceInfo> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let devices = nusb::list_devices().context("failed to list USB devices")?;
        if let Some(dev) = devices
            .into_iter()
            .find(|d| d.vendor_id() == USB_VID_AGILENT && d.product_id() == pid)
        {
            return Ok(dev);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for device 0x0957:{pid:#06x} to appear"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Poll for any 82357B PID (pre-init or post-firmware), returning the first match.
pub async fn wait_for_any_82357b_pid() -> Result<(nusb::DeviceInfo, u16)> {
    loop {
        let devices = nusb::list_devices().context("failed to list USB devices")?;
        for dev in devices {
            if dev.vendor_id() == USB_VID_AGILENT {
                let pid = dev.product_id();
                if pid == USB_PID_82357B || pid == USB_PID_82357B_PREINIT {
                    return Ok((dev, pid));
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Full startup sequence: firmware upload if needed, returning an open `UsbTransport`.
/// Implements the 82357B double-upload quirk.
pub async fn initialize_device(timeout_ms: u32) -> Result<UsbTransport> {
    let (dev_info, pid) = find_device()?;

    if pid == USB_PID_82357B {
        info!("82357B already firmware-loaded (PID 0x0718), skipping upload");
        return open_transport(dev_info, timeout_ms).await;
    }

    info!("82357B pre-init (PID 0x0518), uploading firmware");

    let mut current = dev_info;
    for attempt in 1..=2u32 {
        let device = current
            .open()
            .with_context(|| format!("failed to open pre-init device (attempt {attempt})"))?;
        crate::firmware::upload_firmware(&device)
            .await
            .with_context(|| format!("firmware upload failed (attempt {attempt})"))?;
        info!(attempt, "upload done, waiting for renumeration");

        // Device handle becomes invalid once firmware releases reset; drop it.
        drop(device);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let (new_info, new_pid) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_any_82357b_pid(),
        )
        .await
        .with_context(|| format!("timeout waiting for renumeration on attempt {attempt}"))??;

        if new_pid == USB_PID_82357B {
            info!(attempt, "device came up as 0x0718");
            return open_transport(new_info, timeout_ms).await;
        }

        info!(attempt, "device still 0x0518 — double-upload quirk, retrying");
        current = new_info;
    }
    anyhow::bail!("device still pre-init (0x0518) after two upload attempts")
}
