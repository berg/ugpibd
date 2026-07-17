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

use anyhow::Result;

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
