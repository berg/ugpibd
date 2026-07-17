// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use std::sync::Arc;

use anyhow::{Context, Result};
use nusb::transfer::{ControlIn, ControlType, Recipient, RequestBuffer};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use super::gpib::Transport;
use super::protocol::*;

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
        let notify = write_complete.clone();
        let irq_iface = interface.clone();

        let irq_task = tokio::spawn(async move {
            interrupt_poller(irq_iface, irq_in_ep, notify).await;
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

/// What the interrupt poller should do after a failed transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollerAction {
    /// Clear the endpoint halt, back off, and resubmit — the error is
    /// transient (idle/not-responding, stall, or a one-off fault).
    Recover,
    /// Stop the poller: the queue was cancelled (shutdown) or the device is
    /// gone, neither of which resubmitting can fix.
    Stop,
}

/// Decide how to react to an interrupt-endpoint transfer error.
fn classify_error(e: &nusb::transfer::TransferError) -> PollerAction {
    use nusb::transfer::TransferError::*;
    match e {
        Cancelled | Disconnected => PollerAction::Stop,
        // `Unknown` is the macOS IOKit catch-all (e.g. kIOReturnNotResponding
        // after the bus goes idle); `Stall`/`Fault` clear with a halt reset.
        Stall | Fault | Unknown => PollerAction::Recover,
    }
}

/// Backoff bounds for recovering the interrupt endpoint after a transient
/// error. Starts short so a single idle hiccup re-arms almost immediately,
/// and caps so a persistently failing endpoint doesn't hot-loop.
const IRQ_RECOVER_MIN: std::time::Duration = std::time::Duration::from_millis(50);
const IRQ_RECOVER_MAX: std::time::Duration = std::time::Duration::from_secs(1);

async fn interrupt_poller(interface: nusb::Interface, endpoint: u8, notify: Arc<Notify>) {
    let mut backoff = IRQ_RECOVER_MIN;
    'recover: loop {
        let mut queue = interface.interrupt_in_queue(endpoint);
        loop {
            queue.submit(RequestBuffer::new(INTERRUPT_BUF_LEN));
            let completion = queue.next_complete().await;
            match completion.status {
                Ok(_) => {
                    backoff = IRQ_RECOVER_MIN; // healthy again; reset backoff
                    let flags = completion.data.first().copied().unwrap_or(0);
                    debug!(flags = ?format_args!("{:#04x}", flags), "interrupt");
                    if flags & (1 << AIF_WRITE_COMPLETE_BN) != 0 {
                        notify.notify_one();
                    }
                }
                Err(e) => match classify_error(&e) {
                    PollerAction::Stop => {
                        warn!("interrupt endpoint error: {e} — stopping poller");
                        break 'recover;
                    }
                    PollerAction::Recover => {
                        warn!("interrupt endpoint error: {e} — recovering in {backoff:?}");
                        break; // drop the queue, then clear halt + back off
                    }
                },
            }
        }

        // Recovery: drop the stale queue (done above), clear any halt the
        // device may have raised, wait out the backoff, then resubmit.
        let _ = interface.clear_halt(endpoint);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(IRQ_RECOVER_MAX);
    }
}

impl Transport for UsbTransport {
    async fn write_bulk(&self, data: &[u8]) -> Result<()> {
        debug!(len = data.len(), first = ?&data[..data.len().min(8)], "bulk-out");
        let mut queue = self.interface.bulk_out_queue(self.bulk_out_ep);
        queue.submit(data.to_vec());
        let completion = queue.next_complete().await;
        completion
            .status
            .map_err(|e| anyhow::anyhow!("bulk-out failed: {e}"))?;
        Ok(())
    }

    async fn read_bulk(&self, max_len: usize) -> Result<Vec<u8>> {
        let mut queue = self.interface.bulk_in_queue(self.bulk_in_ep);
        queue.submit(RequestBuffer::new(max_len));
        let completion = queue.next_complete().await;
        completion
            .status
            .map_err(|e| anyhow::anyhow!("bulk-in failed: {e}"))?;
        debug!(
            len = completion.data.len(),
            first = ?&completion.data[..completion.data.len().min(8)],
            "bulk-in"
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

    async fn drain_write_complete(&self) {
        // Consume any pending notification without waiting. `notified` without
        // `.await` doesn't consume; but a quick poll+ready check via a 0ms
        // timeout does. Notify permits cap at 1, so one drain is enough.
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(0),
            self.write_complete.notified(),
        )
        .await;
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
    anyhow::bail!("no Agilent/Keysight 82357B found (expected VID 0x0957, PID 0x0518 or 0x0718)")
}

/// Open device, claim interface 0, return UsbTransport wired to the 82357B endpoints.
pub async fn open_transport(dev_info: nusb::DeviceInfo, timeout_ms: u32) -> Result<UsbTransport> {
    let device = dev_info.open().context("failed to open USB device")?;
    let interface = device.claim_interface(0).context(
        "failed to claim interface 0 — is the kernel driver loaded? \
         See blacklist instructions in README.md",
    )?;

    // Clear residual halt conditions on the bulk pipes before first use.
    let _ = interface.clear_halt(EP_BULK_IN);
    let _ = interface.clear_halt(EP_82357B_BULK_OUT);

    // Drain any stale bulk-in data left over from a prior session. Retry a few
    // times in case the data arrives slightly delayed. Each submit that times
    // out is simply cancelled when its Queue drops.
    for _ in 0..3 {
        let drain = async {
            let mut queue = interface.bulk_in_queue(EP_BULK_IN);
            queue.submit(nusb::transfer::RequestBuffer::new(0x40));
            let completion = queue.next_complete().await;
            if let Ok(()) = completion.status {
                if !completion.data.is_empty() {
                    debug!(len = completion.data.len(), "drained stale bulk-in data");
                    return true;
                }
            }
            false
        };
        let got_data = tokio::time::timeout(std::time::Duration::from_millis(100), drain)
            .await
            .unwrap_or(false);
        if !got_data {
            break;
        }
    }

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
pub async fn wait_for_pid(pid: u16, timeout: std::time::Duration) -> Result<nusb::DeviceInfo> {
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
            anyhow::bail!("timed out waiting for device 0x0957:{pid:#06x} to appear");
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
        let old_bus = current.bus_number();
        let old_addr = current.device_address();

        let device = current
            .open()
            .with_context(|| format!("failed to open pre-init device (attempt {attempt})"))?;
        super::firmware::upload_firmware(&device)
            .await
            .with_context(|| format!("firmware upload failed (attempt {attempt})"))?;
        info!(attempt, "upload done, waiting for renumeration");

        // Device handle becomes invalid once firmware releases reset; drop it.
        drop(device);

        // Wait for the old bus+address to actually go away before accepting a
        // new match — otherwise we race with the kernel still holding the
        // pre-renumeration handle.
        let (new_info, new_pid) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait_for_renumeration(old_bus, old_addr),
        )
        .await
        .with_context(|| format!("timeout waiting for renumeration on attempt {attempt}"))??;

        if new_pid == USB_PID_82357B {
            info!(attempt, "device came up as 0x0718");
            // Small settle so the interface descriptors are readable.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            return open_transport(new_info, timeout_ms).await;
        }

        info!(
            attempt,
            "device still 0x0518 — double-upload quirk, retrying"
        );
        current = new_info;
    }
    anyhow::bail!("device still pre-init (0x0518) after two upload attempts")
}

/// Wait for the pre-firmware device at `(old_bus, old_addr)` to disappear, then
/// poll for any 82357B PID to appear (with a new bus/address or the same one).
async fn wait_for_renumeration(old_bus: u8, old_addr: u8) -> Result<(nusb::DeviceInfo, u16)> {
    // Phase 1: wait until the old device address is gone (or at minimum 200ms settle)
    let phase1_start = std::time::Instant::now();
    loop {
        let devices: Vec<_> = nusb::list_devices()
            .context("failed to list USB devices")?
            .collect();
        let still_present = devices
            .iter()
            .any(|d| d.bus_number() == old_bus && d.device_address() == old_addr);
        if !still_present {
            break;
        }
        if phase1_start.elapsed() >= std::time::Duration::from_secs(3) {
            break; // fallback: assume the device re-used the same address
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Give the device a moment to settle after re-enumeration.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Phase 2: wait for any 82357B PID to appear.
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

#[cfg(test)]
mod tests {
    use super::{classify_error, PollerAction};
    use nusb::transfer::TransferError;

    #[test]
    fn transient_errors_recover() {
        // The macOS IOKit backend collapses idle/not-responding/timeout into
        // `Unknown`; a stall or transient fault is likewise recoverable by
        // clearing the halt and resubmitting. None of these may kill the
        // poller — that's the "must restart ugpibd after idle" bug.
        assert_eq!(
            classify_error(&TransferError::Unknown),
            PollerAction::Recover
        );
        assert_eq!(classify_error(&TransferError::Stall), PollerAction::Recover);
        assert_eq!(classify_error(&TransferError::Fault), PollerAction::Recover);
    }

    #[test]
    fn terminal_errors_stop() {
        // Cancelled means the queue was dropped (shutdown); Disconnected means
        // the device is physically gone — neither is fixable by resubmitting.
        assert_eq!(
            classify_error(&TransferError::Cancelled),
            PollerAction::Stop
        );
        assert_eq!(
            classify_error(&TransferError::Disconnected),
            PollerAction::Stop
        );
    }
}
