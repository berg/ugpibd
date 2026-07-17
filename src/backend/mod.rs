// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// The pluggable GPIB adapter abstraction. A `GpibBackend` is one physical
// USB-GPIB adapter driven from userspace; the network front-ends (Prologix,
// HiSLIP) speak only to this trait, so a new adapter is added by implementing
// it rather than by touching the servers.
//
// This sits *above* the adapter-specific machinery: the 82357B's FX2 framing,
// TMS9914 register pokes, firmware upload, and USB discovery are all private
// to its backend. The trait exposes only the generic IEEE-488 operations the
// front-ends actually consume.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::info;

pub mod agilent_82357b;
pub mod ni_usb_hs;

/// The daemon shares one opened adapter across both front-ends behind this.
pub type SharedBackend = Arc<Mutex<dyn GpibBackend>>;

/// A single GPIB controller adapter, addressing instruments on its bus by
/// primary address (`pad`). Methods take `&mut self`; the daemon shares one
/// instance across both front-ends behind an `Arc<Mutex<dyn GpibBackend>>`.
#[async_trait::async_trait]
pub trait GpibBackend: Send + Sync {
    /// Bring the controller up as system controller: reset, assert IFC, REN.
    /// `my_pad` is the controller's own primary address (conventionally 0).
    async fn init(&mut self, my_pad: u8) -> Result<()>;

    /// Address the instrument at `pad` as listener and write `data`, asserting
    /// EOI on the final byte when `send_eoi` is set.
    async fn write(&mut self, pad: u8, data: &[u8], send_eoi: bool) -> Result<()>;

    /// Address the instrument at `pad` as talker and read up to `max_len`
    /// bytes. Returns the data and whether the message ended (EOI/EOS seen).
    async fn read(&mut self, pad: u8, max_len: usize) -> Result<(Vec<u8>, bool)>;

    /// Selected Device Clear to the instrument at `pad`.
    async fn device_clear(&mut self, pad: u8) -> Result<()>;

    /// Group Execute Trigger to the instrument at `pad`.
    async fn trigger(&mut self, pad: u8) -> Result<()>;

    /// Pulse Interface Clear, returning the bus to idle.
    async fn ifc(&mut self) -> Result<()>;

    /// Assert or deassert Remote Enable.
    async fn ren(&mut self, enable: bool) -> Result<()>;

    /// Serial-poll the instrument at `pad` and return its status byte. The
    /// default returns 0 for adapters that don't implement serial poll yet;
    /// this backs the HiSLIP `get_status` operation.
    async fn serial_poll(&mut self, _pad: u8) -> Result<u8> {
        Ok(0)
    }

    /// Configure the end-of-string terminator used when reading.
    fn set_eos(&mut self, eos_char: u8, enabled: bool);

    /// Set the per-operation GPIB timeout in milliseconds.
    fn set_timeout(&mut self, timeout_ms: u32);

    /// Stable identifier for this adapter kind (e.g. `"agilent-82357b"`).
    fn name(&self) -> &'static str;
}

/// The set of adapter kinds this build knows how to drive. Each variant maps to
/// a submodule providing its id, USB VID/PID table, and `open()` constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Agilent82357b,
    NiUsbHs,
}

impl BackendKind {
    /// Every known backend, in preference order for auto-detection.
    pub const ALL: &'static [BackendKind] = &[BackendKind::Agilent82357b, BackendKind::NiUsbHs];

    /// Stable `--backend` identifier.
    pub fn id(self) -> &'static str {
        match self {
            BackendKind::Agilent82357b => agilent_82357b::ID,
            BackendKind::NiUsbHs => ni_usb_hs::ID,
        }
    }

    /// Human-readable description for `--backend list`.
    pub fn description(self) -> &'static str {
        match self {
            BackendKind::Agilent82357b => agilent_82357b::DESCRIPTION,
            BackendKind::NiUsbHs => ni_usb_hs::DESCRIPTION,
        }
    }

    /// (VID, PID) pairs whose presence indicates this adapter.
    pub fn usb_ids(self) -> &'static [(u16, u16)] {
        match self {
            BackendKind::Agilent82357b => agilent_82357b::USB_IDS,
            BackendKind::NiUsbHs => ni_usb_hs::USB_IDS,
        }
    }

    /// Resolve a `--backend` id string to a kind.
    pub fn from_id(id: &str) -> Option<BackendKind> {
        BackendKind::ALL.iter().copied().find(|k| k.id() == id)
    }

    /// Open, initialize, and return the adapter ready for use.
    pub async fn open(self, timeout_ms: u32) -> Result<SharedBackend> {
        match self {
            BackendKind::Agilent82357b => agilent_82357b::open(timeout_ms).await,
            BackendKind::NiUsbHs => ni_usb_hs::open(timeout_ms).await,
        }
    }

    /// Whether any currently-connected USB device matches this adapter.
    fn is_present(self) -> Result<bool> {
        let ids = self.usb_ids();
        for dev in nusb::list_devices().context("failed to list USB devices")? {
            if ids.contains(&(dev.vendor_id(), dev.product_id())) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Comma-separated list of known backend ids, for help and error messages.
pub fn known_ids() -> String {
    BackendKind::ALL
        .iter()
        .map(|k| k.id())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Auto-detect exactly one connected supported adapter by USB VID/PID.
/// Errors if none — or more than one — is present.
pub fn detect() -> Result<BackendKind> {
    let mut found = Vec::new();
    for &kind in BackendKind::ALL {
        if kind.is_present()? {
            found.push(kind);
        }
    }
    match found.as_slice() {
        [] => anyhow::bail!(
            "no supported USB-GPIB adapter detected (known backends: {})",
            known_ids()
        ),
        [one] => Ok(*one),
        multiple => {
            let ids = multiple
                .iter()
                .map(|k| k.id())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "multiple supported adapters present ({ids}); select one with --backend <id>"
            )
        }
    }
}

/// Open a backend. `selection` is `None` to auto-detect, or `Some(id)` to force
/// a specific one.
pub async fn open_selected(selection: Option<&str>, timeout_ms: u32) -> Result<SharedBackend> {
    let kind = match selection {
        None => detect()?,
        Some(id) => BackendKind::from_id(id)
            .with_context(|| format!("unknown backend {id:?} (known: {})", known_ids()))?,
    };
    info!("using backend {}", kind.id());
    kind.open(timeout_ms).await
}
