// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// The Agilent/Keysight 82357A/B family. Both adapters are FX2LP dongles with a
// TMS9914 GPIB controller and share the same register protocol and init
// sequence (the kernel `agilent_82357a` driver handles both); they differ only
// in USB product ids, two endpoint addresses, and the firmware image. The wire
// protocol, firmware upload, USB discovery, and controller logic live in the
// submodules below; a `Model` descriptor selects the per-adapter parameters.
//
// The 82357B is fully supported and hardware-tested. The 82357A reuses the
// same proven protocol; both models' firmware images are bundled, so a cold
// adapter (pre-firmware) is uploaded automatically. The only per-model wrinkle
// is the CPU-reset register address (see `Model::cpucs_addr`) — the 82357A is a
// first-gen EZ-USB (AN2131) part and remains untested on real hardware.

pub mod firmware;
pub mod gpib;
pub mod protocol;
pub mod usb;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{info, warn};

use self::gpib::GpibController;
use self::protocol::*;
use crate::backend::SharedBackend;

/// Per-adapter parameters distinguishing the 82357A from the 82357B.
pub struct Model {
    /// Stable `--backend` id.
    pub id: &'static str,
    /// Human-readable description for `--backend list`.
    pub description: &'static str,
    /// (VID, PID) pairs this adapter presents (pre- and post-firmware).
    pub usb_ids: &'static [(u16, u16)],
    /// Product id before firmware is uploaded.
    pub pid_preinit: u16,
    /// Product id once firmware is running.
    pub pid_ready: u16,
    /// Bulk-OUT endpoint address.
    pub bulk_out_ep: u8,
    /// Interrupt-IN endpoint address.
    pub irq_in_ep: u8,
    /// Firmware image to upload to a cold adapter, if bundled.
    pub firmware: Option<&'static [u8]>,
    /// CPU control/status register that resets the 8051 during firmware upload:
    /// `0xE600` on the FX2 (82357B), `0x7F92` on the first-gen EZ-USB (82357A).
    pub cpucs_addr: u16,
}

pub const MODEL_82357B: Model = Model {
    id: "agilent-82357b",
    description: "Agilent/Keysight 82357B USB-GPIB adapter",
    usb_ids: &[
        (USB_VID_AGILENT, USB_PID_82357B_PREINIT),
        (USB_VID_AGILENT, USB_PID_82357B),
    ],
    pid_preinit: USB_PID_82357B_PREINIT,
    pid_ready: USB_PID_82357B,
    bulk_out_ep: EP_82357B_BULK_OUT,
    irq_in_ep: EP_82357B_IRQ_IN,
    firmware: Some(firmware::FIRMWARE_82357B),
    cpucs_addr: 0xE600,
};

pub const MODEL_82357A: Model = Model {
    id: "agilent-82357a",
    description: "Agilent 82357A USB-GPIB adapter (experimental; hardware-untested)",
    usb_ids: &[
        (USB_VID_AGILENT, USB_PID_82357A_PREINIT),
        (USB_VID_AGILENT, USB_PID_82357A),
    ],
    pid_preinit: USB_PID_82357A_PREINIT,
    pid_ready: USB_PID_82357A,
    bulk_out_ep: EP_82357A_BULK_OUT,
    irq_in_ep: EP_82357A_IRQ_IN,
    firmware: Some(firmware::FIRMWARE_82357A),
    cpucs_addr: 0x7F92,
};

/// Discover, firmware-load if needed, open, and initialize the adapter.
/// `port` restricts the search to the device at that USB port id.
pub async fn open(
    model: &'static Model,
    timeout_ms: u32,
    port: Option<&str>,
) -> Result<SharedBackend> {
    info!("opening {}", model.description);
    let transport = usb::initialize_device(model, timeout_ms, port).await?;
    info!("USB device open");

    let mut ctrl = GpibController::new(transport, timeout_ms);

    // Try up to 3 times: abort -> init. If init fails it's usually because
    // the device is holding stale state from a prior session; another abort
    // (flush + finalize) typically unsticks it.
    let mut last_err = None;
    for attempt in 1..=3 {
        let _ = ctrl.abort(true).await; // flush pending
        let _ = ctrl.abort(false).await; // finalize
        match ctrl.init(0).await {
            Ok(()) => {
                info!("GPIB controller initialized (attempt {attempt})");
                last_err = None;
                break;
            }
            Err(e) => {
                warn!("init attempt {attempt} failed: {e:#}");
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }

    // Concrete GpibController<UsbTransport> coerces to the trait object here.
    let backend: SharedBackend = Arc::new(Mutex::new(ctrl));
    Ok(backend)
}
