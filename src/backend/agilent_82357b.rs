// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Entry point for the Agilent/Keysight 82357B backend. The wire protocol,
// firmware upload, USB discovery, and TMS9914 controller logic live in
// `crate::{protocol, firmware, usb, gpib}`; this module just wires them into a
// ready-to-use `GpibBackend` for the registry.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::backend::SharedBackend;
use crate::gpib::GpibController;
use crate::protocol::{USB_PID_82357B, USB_PID_82357B_PREINIT, USB_VID_AGILENT};

/// Stable backend identifier used by `--backend`.
pub const ID: &str = "agilent-82357b";

/// Human-readable description shown by `--backend list`.
pub const DESCRIPTION: &str = "Agilent/Keysight 82357B USB-GPIB adapter";

/// (VID, PID) pairs this adapter presents: pre-firmware and post-firmware.
pub const USB_IDS: &[(u16, u16)] = &[
    (USB_VID_AGILENT, USB_PID_82357B_PREINIT),
    (USB_VID_AGILENT, USB_PID_82357B),
];

/// Discover, firmware-load if needed, open, and initialize a 82357B.
pub async fn open(timeout_ms: u32) -> Result<SharedBackend> {
    info!("opening Agilent/Keysight 82357B");
    let transport = crate::usb::initialize_device(timeout_ms).await?;
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
