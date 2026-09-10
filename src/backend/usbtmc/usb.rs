// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// nusb transport for a USBTMC interface: discovery by interface class,
// endpoint wiring from the descriptor (USBTMC fixes none of the addresses),
// bulk and class-control transfers, and the interrupt-IN reader that carries
// USB488 status bytes and service requests.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nusb::descriptors::TransferType;
use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlType, Direction, In, Interrupt, Out, Recipient,
};
use nusb::{Endpoint, MaybeFuture};
use tracing::{debug, info, warn};

use super::protocol::{
    Recipient as TmcRecipient, NOTIFY_SRQ, USB_CLASS_APPLICATION_SPECIFIC, USB_PROTOCOL_USB488,
    USB_SUBCLASS_USBTMC,
};
use super::TmcTransport;

/// Class control requests are short handshakes; anything approaching this
/// means the device is not answering.
const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Largest single URB. usbfs and libusb both split at this size and some
/// kernels refuse larger bulk URBs outright, so a transfer is submitted as a
/// sequence of these.
const MAX_URB: usize = 16 * 1024;

/// How many service-request notifications to buffer. SRQ is a level, not a
/// count, so a lagging subscriber only needs to learn that someone asked.
const SRQ_CHANNEL_CAPACITY: usize = 16;

/// The USBTMC interface a device exposes, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbtmcInterface {
    pub number: u8,
    /// `0x01` for USB488; `0x00` for a plain USBTMC interface.
    pub protocol: u8,
}

impl UsbtmcInterface {
    pub fn is_usb488(&self) -> bool {
        self.protocol == USB_PROTOCOL_USB488
    }
}

/// The USBTMC interface on `dev`, found by class rather than vendor id: every
/// compliant instrument advertises class `fe`, subclass `03`, so there is no
/// id table to maintain.
pub fn usbtmc_interface(dev: &nusb::DeviceInfo) -> Option<UsbtmcInterface> {
    dev.interfaces()
        .find(|i| {
            i.class() == USB_CLASS_APPLICATION_SPECIFIC && i.subclass() == USB_SUBCLASS_USBTMC
        })
        .map(|i| UsbtmcInterface {
            number: i.interface_number(),
            protocol: i.protocol(),
        })
}

struct BulkIo {
    out: Endpoint<Bulk, Out>,
    r#in: Endpoint<Bulk, In>,
}

pub struct UsbtmcTransport {
    interface: nusb::Interface,
    /// Held so the device stays open for the transport's lifetime.
    _device: nusb::Device,
    io: tokio::sync::Mutex<BulkIo>,
    interface_number: u8,
    bulk_in_addr: u8,
    bulk_out_addr: u8,
    timeout_ms: AtomicU32,
    srq: tokio::sync::broadcast::Sender<()>,
    /// READ_STATUS_BYTE replies from the interrupt reader, as `(bTag, STB)`,
    /// when the interface has an interrupt endpoint.
    status_bytes: Option<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<(u8, u8)>>>,
    /// Aborted on drop.
    _reader_task: Option<tokio::task::JoinHandle<()>>,
}

impl UsbtmcTransport {
    /// Find a USBTMC device (at `port` when given), detach the kernel driver
    /// from its USBTMC interface, claim it, and wire up its endpoints.
    pub async fn open(port: Option<&str>) -> Result<Self> {
        let (dev_info, tmc) = find_device(port)?;
        let device = dev_info
            .open()
            .wait()
            .context("failed to open USBTMC device")?;
        // The kernel's own `usbtmc` class driver will have bound the
        // interface; take it from that one interface only, and nusb hands it
        // back when the last handle drops.
        let interface = device
            .detach_and_claim_interface(tmc.number)
            .wait()
            .with_context(|| {
                format!(
                    "failed to claim USBTMC interface {} — is another process using the device?",
                    tmc.number
                )
            })?;

        let desc = interface
            .descriptor()
            .context("USBTMC interface has no descriptor")?;
        let mut bulk_in = None;
        let mut bulk_out = None;
        let mut interrupt_in = None;
        for ep in desc.endpoints() {
            match (ep.transfer_type(), ep.direction()) {
                (TransferType::Bulk, Direction::In) => bulk_in = Some(ep.address()),
                (TransferType::Bulk, Direction::Out) => bulk_out = Some(ep.address()),
                (TransferType::Interrupt, Direction::In) => interrupt_in = Some(ep.address()),
                _ => {}
            }
        }
        let bulk_in_addr = bulk_in.context("USBTMC interface has no bulk-IN endpoint")?;
        let bulk_out_addr = bulk_out.context("USBTMC interface has no bulk-OUT endpoint")?;
        info!(
            interface = tmc.number,
            usb488 = tmc.is_usb488(),
            bulk_out = format!("{bulk_out_addr:#04x}"),
            bulk_in = format!("{bulk_in_addr:#04x}"),
            interrupt_in = interrupt_in.map(|a| format!("{a:#04x}")),
            product = dev_info.product_string(),
            "usbtmc device open"
        );

        let io = tokio::sync::Mutex::new(BulkIo {
            out: interface
                .endpoint::<Bulk, Out>(bulk_out_addr)
                .with_context(|| format!("open bulk-out endpoint {bulk_out_addr:#04x}"))?,
            r#in: interface
                .endpoint::<Bulk, In>(bulk_in_addr)
                .with_context(|| format!("open bulk-in endpoint {bulk_in_addr:#04x}"))?,
        });

        let (srq, _) = tokio::sync::broadcast::channel(SRQ_CHANNEL_CAPACITY);
        let (status_bytes, reader_task) = match interrupt_in {
            Some(addr) => {
                let ep = interface
                    .endpoint::<Interrupt, In>(addr)
                    .with_context(|| format!("open interrupt endpoint {addr:#04x}"))?;
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let task = tokio::spawn(interrupt_reader(ep, srq.clone(), tx));
                (Some(tokio::sync::Mutex::new(rx)), Some(task))
            }
            None => (None, None),
        };

        Ok(Self {
            interface,
            _device: device,
            io,
            interface_number: tmc.number,
            bulk_in_addr,
            bulk_out_addr,
            timeout_ms: AtomicU32::new(3000),
            srq,
            status_bytes,
            _reader_task: reader_task,
        })
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(u64::from(self.timeout_ms.load(Ordering::Relaxed)))
    }
}

/// Locate a USBTMC device, restricted to `port` when given.
fn find_device(port: Option<&str>) -> Result<(nusb::DeviceInfo, UsbtmcInterface)> {
    for dev in nusb::list_devices()
        .wait()
        .context("failed to list USB devices")?
    {
        let Some(tmc) = usbtmc_interface(&dev) else {
            continue;
        };
        if let Some(want) = port {
            if crate::backend::select::port_id(&dev) != want {
                continue;
            }
        }
        return Ok((dev, tmc));
    }
    match port {
        Some(want) => bail!("no USBTMC device found at USB port {want:?}"),
        None => bail!("no USBTMC device found (USB interface class fe, subclass 03)"),
    }
}

/// Cancel and discard everything queued on an endpoint, so a transfer the
/// caller gave up on cannot be handed to the next caller as its reply.
async fn discard_pending<D: nusb::transfer::EndpointDirection>(ep: &mut Endpoint<Bulk, D>) {
    ep.cancel_all();
    while ep.pending() > 0 {
        let c = ep.next_complete().await;
        debug!(
            status = ?c.status,
            len = c.buffer.len(),
            "usbtmc discarded a stale transfer"
        );
    }
}

/// Read the interrupt endpoint for as long as the device is open. USB488
/// packets are two bytes: `bNotify1`, then `bNotify2`. `0x81` announces a
/// service request with the status byte in the second byte; `0x80 | bTag`
/// answers the READ_STATUS_BYTE with that tag.
async fn interrupt_reader(
    mut endpoint: Endpoint<Interrupt, In>,
    srq: tokio::sync::broadcast::Sender<()>,
    status_bytes: tokio::sync::mpsc::UnboundedSender<(u8, u8)>,
) {
    let buf_len = {
        let mps = endpoint.max_packet_size().max(1);
        64usize.div_ceil(mps) * mps
    };
    loop {
        endpoint.submit(Buffer::new(buf_len));
        let completion = endpoint.next_complete().await;
        match completion.status {
            Ok(()) if completion.buffer.len() < 2 => {}
            Ok(()) => {
                let (b1, b2) = (completion.buffer[0], completion.buffer[1]);
                if b1 == NOTIFY_SRQ {
                    debug!(stb = format!("{b2:#04x}"), "usbtmc service request");
                    let _ = srq.send(());
                } else if b1 & 0x80 != 0 {
                    let tag = b1 & 0x7f;
                    debug!(tag, stb = format!("{b2:#04x}"), "usbtmc status byte");
                    if status_bytes.send((tag, b2)).is_err() {
                        return;
                    }
                } else {
                    debug!(b1, b2, "usbtmc ignoring an unknown interrupt packet");
                }
            }
            Err(e) => {
                debug!("usbtmc interrupt read failed, continuing: {e}");
                let _ = endpoint.clear_halt().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

#[async_trait::async_trait]
impl TmcTransport for UsbtmcTransport {
    async fn bulk_out(&self, data: &[u8]) -> Result<()> {
        let timeout = self.timeout();
        let mut io = self.io.lock().await;
        for chunk in data.chunks(MAX_URB) {
            io.out.submit(chunk.to_vec().into());
            let completion = match tokio::time::timeout(timeout, io.out.next_complete()).await {
                Ok(c) => c,
                Err(_) => {
                    discard_pending(&mut io.out).await;
                    bail!("usbtmc bulk-out timed out after {timeout:?}");
                }
            };
            completion
                .status
                .map_err(|e| anyhow::anyhow!("usbtmc bulk-out failed: {e}"))?;
        }
        debug!(len = data.len(), "usbtmc bulk-out");
        Ok(())
    }

    async fn bulk_in(&self, max_len: usize) -> Result<Vec<u8>> {
        let timeout = self.timeout();
        let mut io = self.io.lock().await;
        let mps = io.r#in.max_packet_size().max(1);
        let mut data = Vec::with_capacity(max_len.min(MAX_URB));
        // A transfer ends with a short packet (possibly zero-length) or when
        // it has filled what was asked for. Each URB must request whole
        // packets, so the last one may return a few alignment bytes past
        // `max_len`; the caller's header parsing discards those.
        while data.len() < max_len {
            let want = (max_len - data.len()).min(MAX_URB).div_ceil(mps) * mps;
            io.r#in.submit(Buffer::new(want));
            let completion = match tokio::time::timeout(timeout, io.r#in.next_complete()).await {
                Ok(c) => c,
                Err(_) => {
                    // Leave nothing behind: the transfer is still pending and
                    // would otherwise complete into the next caller's read.
                    discard_pending(&mut io.r#in).await;
                    bail!("usbtmc bulk-in timed out after {timeout:?}");
                }
            };
            completion
                .status
                .map_err(|e| anyhow::anyhow!("usbtmc bulk-in failed: {e}"))?;
            let got = completion.buffer.len();
            data.extend_from_slice(&completion.buffer);
            if got < want {
                break;
            }
        }
        debug!(len = data.len(), "usbtmc bulk-in");
        Ok(data)
    }

    async fn control_in(
        &self,
        recipient: TmcRecipient,
        request: u8,
        value: u16,
        index: u16,
        len: usize,
    ) -> Result<Vec<u8>> {
        let recipient = match recipient {
            TmcRecipient::Interface => Recipient::Interface,
            TmcRecipient::Endpoint => Recipient::Endpoint,
        };
        let reply = self
            .interface
            .control_in(
                ControlIn {
                    control_type: ControlType::Class,
                    recipient,
                    request,
                    value,
                    index,
                    length: len as u16,
                },
                CONTROL_TIMEOUT,
            )
            .await
            .map_err(|e| anyhow::anyhow!("usbtmc control request {request:#04x} failed: {e}"))?;
        debug!(request, value, index, reply = ?reply, "usbtmc control-in");
        Ok(reply)
    }

    async fn reset_bulk_in(&self) -> Result<()> {
        let mut io = self.io.lock().await;
        discard_pending(&mut io.r#in).await;
        io.r#in
            .clear_halt()
            .await
            .map_err(|e| anyhow::anyhow!("usbtmc clear bulk-in halt: {e}"))
    }

    async fn reset_bulk_out(&self) -> Result<()> {
        let mut io = self.io.lock().await;
        discard_pending(&mut io.out).await;
        io.out
            .clear_halt()
            .await
            .map_err(|e| anyhow::anyhow!("usbtmc clear bulk-out halt: {e}"))
    }

    fn interface_number(&self) -> u8 {
        self.interface_number
    }

    fn bulk_in_address(&self) -> u8 {
        self.bulk_in_addr
    }

    fn bulk_out_address(&self) -> u8 {
        self.bulk_out_addr
    }

    fn set_timeout(&self, timeout_ms: u32) {
        self.timeout_ms.store(timeout_ms, Ordering::Relaxed);
    }

    fn has_interrupt_in(&self) -> bool {
        self.status_bytes.is_some()
    }

    /// Wait for the notification tagged `tag`. Earlier tags still queued are
    /// answers to polls whose caller gave up, and are dropped.
    async fn status_byte_notification(&self, tag: u8, wait: Duration) -> Result<u8> {
        let Some(rx) = &self.status_bytes else {
            bail!("this interface has no interrupt endpoint");
        };
        let mut rx = rx.lock().await;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let next = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .context("timed out waiting for the status byte on the interrupt endpoint")?;
            match next {
                Some((t, stb)) if t == tag => return Ok(stb),
                Some((t, _)) => {
                    warn!(
                        got = t,
                        want = tag,
                        "usbtmc dropping a stale status-byte notification"
                    )
                }
                None => bail!("usbtmc interrupt reader has stopped"),
            }
        }
    }

    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        self.status_bytes.as_ref().map(|_| self.srq.subscribe())
    }
}
