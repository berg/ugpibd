// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use std::sync::Arc;

use anyhow::{Context, Result};
use nusb::transfer::{Buffer, Bulk, ControlIn, ControlType, In, Interrupt, Out, Recipient};
use nusb::{Endpoint, MaybeFuture};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use super::gpib::Transport;
use super::protocol::*;
use super::Model;

/// How many service-request notifications to buffer. SRQ is a level, not a
/// count: a subscriber that lags only needs to learn that *some* device
/// requested service, so a small buffer is plenty and lagging is harmless.
const SRQ_CHANNEL_CAPACITY: usize = 16;

/// Cap on vendor control transfers. These are short handshake exchanges, so
/// anything approaching this means the adapter has stopped answering.
const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// The bulk endpoints, held open for the life of the transport.
///
/// nusb 0.2 makes an endpoint a persistent object rather than something
/// recreated per transfer, which removes the reopen race and lets a stalled
/// transfer be cleared without tearing anything down. They live behind one
/// mutex so a request and its reply cannot be interleaved with anything else.
struct BulkIo {
    out: Endpoint<Bulk, Out>,
    r#in: Endpoint<Bulk, In>,
}

pub struct UsbTransport {
    /// Held only to keep the interface claimed for the transport's lifetime;
    /// all I/O goes through the endpoints opened from it.
    _interface: nusb::Interface,
    device: nusb::Device,
    io: tokio::sync::Mutex<BulkIo>,
    write_complete: Arc<Notify>,
    srq: tokio::sync::broadcast::Sender<()>,
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
    ) -> Result<Self> {
        let write_complete = Arc::new(Notify::new());
        let notify = write_complete.clone();
        let irq_iface = interface.clone();
        let (srq, _) = tokio::sync::broadcast::channel(SRQ_CHANNEL_CAPACITY);
        let srq_tx = srq.clone();

        let irq_ep = irq_iface
            .endpoint::<Interrupt, In>(irq_in_ep)
            .with_context(|| format!("open interrupt endpoint {irq_in_ep:#04x}"))?;
        let irq_task = tokio::spawn(async move {
            interrupt_poller(irq_ep, notify, srq_tx).await;
        });

        let io = tokio::sync::Mutex::new(BulkIo {
            out: interface
                .endpoint::<Bulk, Out>(bulk_out_ep)
                .with_context(|| format!("open bulk-out endpoint {bulk_out_ep:#04x}"))?,
            r#in: interface
                .endpoint::<Bulk, In>(bulk_in_ep)
                .with_context(|| format!("open bulk-in endpoint {bulk_in_ep:#04x}"))?,
        });

        Ok(Self {
            _interface: interface,
            device,
            io,
            write_complete,
            srq,
            timeout_ms,
            _irq_task: irq_task,
        })
    }

    /// Receiver for service-request notifications raised by the adapter's
    /// interrupt endpoint.
    pub fn subscribe_srq(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.srq.subscribe()
    }

    /// Clear residual halts and discard any bulk-in data a previous session
    /// left queued.
    ///
    /// Every operation is a bulk-out followed by the bulk-in carrying its
    /// status. A process that died between the two leaves that reply queued,
    /// and the next session reads it in place of its own — from then on every
    /// reply answers the previous request.
    pub async fn quiesce_bulk(&self) {
        let mut io = self.io.lock().await;
        let _ = MaybeFuture::wait(io.out.clear_halt());
        let _ = MaybeFuture::wait(io.r#in.clear_halt());

        // Retry a few times in case the data arrives slightly delayed.
        for _ in 0..3 {
            let mps = io.r#in.max_packet_size().max(1);
            io.r#in.submit(Buffer::new(0x40usize.div_ceil(mps) * mps));
            let got = match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                io.r#in.next_complete(),
            )
            .await
            {
                Ok(c) if c.status.is_ok() && !c.buffer.is_empty() => {
                    debug!(len = c.buffer.len(), "drained stale bulk-in data");
                    true
                }
                // Timed out: the read stays pending and is picked up by the
                // next one, which is what the endpoint being persistent buys.
                _ => false,
            };
            if !got {
                break;
            }
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
        // after the bus goes idle) and carries the raw code; `Stall`/`Fault`
        // clear with a halt reset. Anything new upstream adds is treated as
        // recoverable rather than fatal.
        _ => PollerAction::Recover,
    }
}

/// Backoff bounds for recovering the interrupt endpoint after a transient
/// error. Starts short so a single idle hiccup re-arms almost immediately,
/// and caps so a persistently failing endpoint doesn't hot-loop.
const IRQ_RECOVER_MIN: std::time::Duration = std::time::Duration::from_millis(50);
const IRQ_RECOVER_MAX: std::time::Duration = std::time::Duration::from_secs(1);

async fn interrupt_poller(
    mut endpoint: Endpoint<Interrupt, In>,
    notify: Arc<Notify>,
    srq: tokio::sync::broadcast::Sender<()>,
) {
    let mut backoff = IRQ_RECOVER_MIN;
    // An IN transfer must request a whole number of max-size packets.
    let buf_len = {
        let mps = endpoint.max_packet_size().max(1);
        INTERRUPT_BUF_LEN.div_ceil(mps) * mps
    };
    'recover: loop {
        loop {
            endpoint.submit(Buffer::new(buf_len));
            let completion = endpoint.next_complete().await;
            match completion.status {
                Ok(_) => {
                    backoff = IRQ_RECOVER_MIN; // healthy again; reset backoff
                    let flags = completion.buffer.first().copied().unwrap_or(0);
                    debug!(flags = ?format_args!("{:#04x}", flags), "interrupt");
                    if flags & (1 << AIF_WRITE_COMPLETE_BN) != 0 {
                        notify.notify_one();
                    }
                    if flags & (1 << AIF_SRQ_BN) != 0 {
                        // No subscribers is the normal case (nothing is
                        // watching for SRQ), so a send error is not a problem.
                        let _ = srq.send(());
                    }
                }
                Err(e) => match classify_error(&e) {
                    PollerAction::Stop => {
                        warn!("interrupt endpoint error: {e} — stopping poller");
                        break 'recover;
                    }
                    PollerAction::Recover => {
                        warn!("interrupt endpoint error: {e} — recovering in {backoff:?}");
                        break; // clear halt + back off, then resubmit
                    }
                },
            }
        }

        // Recovery: clear any halt the device raised, wait out the backoff,
        // then resubmit on the same endpoint.
        let _ = MaybeFuture::wait(endpoint.clear_halt());
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(IRQ_RECOVER_MAX);
    }
}

impl Transport for UsbTransport {
    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        Some(UsbTransport::subscribe_srq(self))
    }

    async fn write_bulk(&self, data: &[u8]) -> Result<()> {
        debug!(len = data.len(), first = ?&data[..data.len().min(8)], "bulk-out");
        let mut io = self.io.lock().await;
        io.out.submit(data.to_vec().into());
        let completion = io.out.next_complete().await;
        completion
            .status
            .map_err(|e| anyhow::anyhow!("bulk-out failed: {e}"))?;
        Ok(())
    }

    async fn read_bulk(&self, max_len: usize) -> Result<Vec<u8>> {
        let mut io = self.io.lock().await;
        // A bulk IN transfer ends at a short packet, so a reply longer than one
        // max-size packet arrives as several completions and must be
        // reassembled; a single read would return just the first packet. Each
        // request is rounded up to a whole number of packets, as the API needs.
        let mps = io.r#in.max_packet_size().max(1);
        let mut data: Vec<u8> = Vec::with_capacity(max_len);

        while data.len() < max_len {
            let want = (max_len - data.len()).max(1).div_ceil(mps) * mps;
            io.r#in.submit(Buffer::new(want));
            let completion = io.r#in.next_complete().await;
            completion
                .status
                .map_err(|e| anyhow::anyhow!("bulk-in failed: {e}"))?;
            let chunk = completion.buffer;
            let short = chunk.len() % mps != 0 || chunk.is_empty();
            data.extend_from_slice(&chunk);
            if short {
                break;
            }
        }

        debug!(
            len = data.len(),
            first = ?&data[..data.len().min(8)],
            "bulk-in"
        );
        Ok(data)
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
            .map_err(|e| anyhow::anyhow!("control-in failed: {e}"))?;
        Ok(completion)
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

/// Find an adapter of `model` in either pre-firmware or post-firmware state,
/// restricted to `port` (USB port id) when given.
pub fn find_device(model: &Model, port: Option<&str>) -> Result<(nusb::DeviceInfo, u16)> {
    let devices = nusb::list_devices()
        .wait()
        .context("failed to list USB devices")?;
    for dev in devices {
        if dev.vendor_id() != USB_VID_AGILENT {
            continue;
        }
        let pid = dev.product_id();
        if pid != model.pid_ready && pid != model.pid_preinit {
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
        Some(want) => anyhow::bail!("no {} found at USB port {want:?}", model.id),
        None => anyhow::bail!(
            "no {} found (expected VID {:#06x}, PID {:#06x} or {:#06x})",
            model.id,
            USB_VID_AGILENT,
            model.pid_preinit,
            model.pid_ready
        ),
    }
}

/// Open device, claim interface 0, return UsbTransport wired to the model's endpoints.
pub async fn open_transport(
    model: &Model,
    dev_info: nusb::DeviceInfo,
    timeout_ms: u32,
) -> Result<UsbTransport> {
    let device = dev_info
        .open()
        .wait()
        .context("failed to open USB device")?;
    let interface = device.claim_interface(0).wait().context(
        "failed to claim interface 0 — is the kernel driver loaded? \
         See blacklist instructions in README.md",
    )?;

    let transport = UsbTransport::new(
        device,
        interface,
        model.bulk_out_ep,
        EP_BULK_IN,
        model.irq_in_ep,
        timeout_ms,
    )?;

    // Clear residual halts and drain stale bulk-in data left by a prior
    // session, using the transport's own endpoints. Opening a second endpoint
    // on the same address to do this fails: an endpoint is exclusively owned,
    // and a drain read that times out is still pending when it drops, so the
    // address stays claimed and the transport cannot open it.
    transport.quiesce_bulk().await;
    Ok(transport)
}

/// Poll for a device with the given PID to appear, up to `timeout`.
pub async fn wait_for_pid(pid: u16, timeout: std::time::Duration) -> Result<nusb::DeviceInfo> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let devices = nusb::list_devices()
            .wait()
            .context("failed to list USB devices")?;
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

/// Full startup sequence: firmware upload if needed, returning an open `UsbTransport`.
/// Implements the 82357B double-upload quirk.
pub async fn initialize_device(
    model: &Model,
    timeout_ms: u32,
    port: Option<&str>,
) -> Result<UsbTransport> {
    let (dev_info, pid) = find_device(model, port)?;

    if pid == model.pid_ready {
        info!(
            "{} already firmware-loaded (PID {:#06x}), skipping upload",
            model.id, model.pid_ready
        );
        return open_transport(model, dev_info, timeout_ms).await;
    }

    let firmware = model.firmware.ok_or_else(|| {
        anyhow::anyhow!(
            "{} is in pre-firmware state (PID {:#06x}) but its firmware image is not bundled; \
             load it externally (e.g. fxload) or connect an already-initialized adapter",
            model.id,
            model.pid_preinit
        )
    })?;

    info!(
        "{} pre-init (PID {:#06x}), uploading firmware",
        model.id, model.pid_preinit
    );

    let mut current = dev_info;
    for attempt in 1..=2u32 {
        let old_bus = current.bus_id().to_string();
        let old_addr = current.device_address();

        let device = current
            .open()
            .wait()
            .with_context(|| format!("failed to open pre-init device (attempt {attempt})"))?;
        super::firmware::upload_firmware(&device, firmware, model.cpucs_addr)
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
            wait_for_renumeration(model, port, old_bus, old_addr),
        )
        .await
        .with_context(|| format!("timeout waiting for renumeration on attempt {attempt}"))??;

        if new_pid == model.pid_ready {
            info!(attempt, "device came up as {:#06x}", model.pid_ready);
            // Small settle so the interface descriptors are readable.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            return open_transport(model, new_info, timeout_ms).await;
        }

        info!(
            attempt,
            "device still {:#06x} — double-upload quirk, retrying", model.pid_preinit
        );
        current = new_info;
    }
    anyhow::bail!(
        "device still pre-init ({:#06x}) after two upload attempts",
        model.pid_preinit
    )
}

/// Wait for the pre-firmware device at `(old_bus, old_addr)` to disappear, then
/// poll for a PID of `model` to appear. When `port` is set, only the device at
/// that USB port id is accepted — the port is stable across renumeration, so
/// this re-finds the *same* physical adapter even when identical units share the
/// bus.
async fn wait_for_renumeration(
    model: &Model,
    port: Option<&str>,
    old_bus: String,
    old_addr: u8,
) -> Result<(nusb::DeviceInfo, u16)> {
    // Phase 1: wait until the old device address is gone (or at minimum 200ms settle)
    let phase1_start = std::time::Instant::now();
    loop {
        let devices: Vec<_> = nusb::list_devices()
            .wait()
            .context("failed to list USB devices")?
            .collect();
        let still_present = devices
            .iter()
            .any(|d| d.bus_id() == old_bus.as_str() && d.device_address() == old_addr);
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

    // Phase 2: wait for any PID of this model to appear.
    loop {
        let devices = nusb::list_devices()
            .wait()
            .context("failed to list USB devices")?;
        for dev in devices {
            if dev.vendor_id() == USB_VID_AGILENT {
                let pid = dev.product_id();
                if pid == model.pid_ready || pid == model.pid_preinit {
                    if let Some(want) = port {
                        if crate::backend::select::port_id(&dev) != want {
                            continue;
                        }
                    }
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
            classify_error(&TransferError::Unknown(0)),
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
