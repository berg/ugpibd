// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// One GPIB instrument on the shared bus, as split primitives.
//
// `write` and `read` are separate operations here on purpose. Fusing them —
// deciding at write time whether a read follows — is a *protocol* concern,
// forced on HiSLIP by its push model, and it is exactly the decision that
// cannot be made correctly for instruments that only produce output once
// addressed to talk (an HP 8594E's `PRINT` sets no status bit; the data does
// not exist until the instrument is addressed). Front-ends whose wire
// protocol carries an explicit read request get to keep that honesty by
// calling `read` when — and only when — the client asked for one.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{broadcast, Mutex, MutexGuard};

use crate::backend::GpibBackend;

/// Upper bound on a single bulk read. Matches the Prologix server's default so
/// behavior is consistent across front-ends.
pub const MAX_READ: usize = 65536;

/// A GPIB instrument addressable at `pad` on the shared bus.
pub struct Instrument {
    ctrl: Arc<Mutex<dyn GpibBackend>>,
    pad: u8,
}

impl Instrument {
    pub fn new(ctrl: Arc<Mutex<dyn GpibBackend>>, pad: u8) -> Self {
        Self { ctrl, pad }
    }

    pub fn pad(&self) -> u8 {
        self.pad
    }

    /// The instrument, not the session or the protocol, is what a lock
    /// protects: two clients that resolve to the same primary address contend,
    /// and two that resolve to different ones do not — across front-ends.
    /// Every spelling that means the same PAD lands on the same key.
    pub fn resource_key(&self) -> String {
        format!("gpib{}", self.pad)
    }

    /// Take the bus for a multi-step transaction. Everything done through the
    /// returned guard is one uninterrupted bus tenure: no other client's
    /// traffic can interleave between, say, a write and the serial poll that
    /// observes its consequences. Single operations can use the one-shot
    /// methods below instead.
    pub async fn hold(&self) -> Held<'_> {
        Held {
            ctrl: self.ctrl.lock().await,
            pad: self.pad,
        }
    }

    /// Send a GPIB trigger (GET) to the instrument.
    pub async fn trigger(&self) -> Result<()> {
        self.hold().await.ctrl.trigger(self.pad).await
    }

    /// Send Selected Device Clear to the instrument.
    pub async fn device_clear(&self) -> Result<()> {
        self.hold().await.ctrl.device_clear(self.pad).await
    }

    /// Drive REN on/off. Bus-wide by nature of the line.
    pub async fn ren(&self, enable: bool) -> Result<()> {
        self.hold().await.ctrl.ren(enable).await
    }

    /// Assert REN and address this instrument, putting it into remote state.
    pub async fn go_to_remote(&self) -> Result<()> {
        self.hold().await.ctrl.go_to_remote(self.pad).await
    }

    /// Send an addressed Go To Local to this instrument.
    pub async fn go_to_local(&self) -> Result<()> {
        self.hold().await.ctrl.go_to_local(self.pad).await
    }

    /// Send Local Lockout, which the standard defines only bus-wide.
    pub async fn local_lockout(&self) -> Result<()> {
        self.hold().await.ctrl.local_lockout().await
    }

    /// Read the instrument's serial-poll status byte.
    pub async fn serial_poll(&self) -> Result<u8> {
        self.hold().await.ctrl.serial_poll(self.pad).await
    }

    /// Whether any device on the bus is currently asserting SRQ — a level
    /// read, not an event.
    pub async fn srq_asserted(&self) -> Result<bool> {
        self.hold().await.ctrl.srq_asserted().await
    }

    /// Subscribe to service requests. `None` means the underlying adapter has
    /// no way to report SRQ.
    pub async fn subscribe_srq(&self) -> Option<broadcast::Receiver<()>> {
        self.hold().await.ctrl.subscribe_srq()
    }
}

/// The bus, held. Operations are the same as [`Instrument`]'s plus the data
/// path; the difference is tenure — nothing else touches the bus until this
/// is dropped.
pub struct Held<'a> {
    ctrl: MutexGuard<'a, dyn GpibBackend>,
    pad: u8,
}

impl Held<'_> {
    /// Subscribe to service requests while already on the bus, so a request
    /// raised by traffic later in this same tenure is in the queue by the
    /// time it is checked.
    pub fn subscribe_srq(&self) -> Option<broadcast::Receiver<()>> {
        self.ctrl.subscribe_srq()
    }

    /// Write `data` to the instrument, EOI on the last byte if `send_eoi`.
    pub async fn write(&mut self, data: &[u8], send_eoi: bool) -> Result<()> {
        self.ctrl.write(self.pad, data, send_eoi).await
    }

    /// Address the instrument to talk and read until EOI, `max_len` bytes,
    /// the configured end-of-string character, or the bus timeout. Returns
    /// the data and whether END (EOI) terminated it. `max_len` is clamped to
    /// [`MAX_READ`].
    pub async fn read(&mut self, max_len: usize) -> Result<(Vec<u8>, bool)> {
        self.ctrl.read(self.pad, max_len.min(MAX_READ)).await
    }

    /// Set the bus timeout for operations later in this tenure. The caller
    /// owns restoration: nothing here remembers what the timeout was, and a
    /// tenure that changes it must put the daemon default back before the
    /// next front-end's traffic runs at the wrong one.
    pub fn set_timeout(&mut self, timeout_ms: u32) {
        self.ctrl.set_timeout(timeout_ms);
    }

    /// The current end-of-string configuration, for save/restore around a
    /// per-operation terminator (VXI-11 termChar). Restoration matters
    /// beyond this tenure: Prologix `++eos` state is persistent by that
    /// protocol's contract.
    pub fn eos(&self) -> (u8, bool) {
        self.ctrl.eos()
    }

    /// Configure the end-of-string terminator used when reading.
    pub fn set_eos(&mut self, eos_char: u8, enabled: bool) {
        self.ctrl.set_eos(eos_char, enabled);
    }

    /// The adapter's read-slicing requirement; see the trait.
    pub fn read_slice_ms(&self) -> Option<u32> {
        self.ctrl.read_slice_ms()
    }

    /// Read the instrument's serial-poll status byte.
    pub async fn serial_poll(&mut self) -> Result<u8> {
        self.ctrl.serial_poll(self.pad).await
    }

    /// Whether any device on the bus is currently asserting SRQ.
    pub async fn srq_asserted(&mut self) -> Result<bool> {
        self.ctrl.srq_asserted().await
    }

    /// Send raw GPIB command bytes (ATN asserted). Interface-level: the
    /// caller chooses the bytes, addressing included.
    pub async fn send_bus_command(&mut self, cmds: &[u8]) -> Result<()> {
        self.ctrl.send_bus_command(cmds).await
    }

    /// Drive the ATN line: take control (true) or standby (false).
    pub async fn set_atn(&mut self, assert: bool) -> Result<()> {
        self.ctrl.set_atn(assert).await
    }

    /// A live read of the GPIB control lines.
    pub async fn bus_lines(&mut self) -> Result<crate::backend::BusLines> {
        self.ctrl.bus_lines().await
    }

    /// Pulse Interface Clear.
    pub async fn ifc(&mut self) -> Result<()> {
        self.ctrl.ifc().await
    }

    /// The controller's own primary address.
    pub fn controller_pad(&self) -> u8 {
        self.ctrl.controller_pad()
    }

    /// Re-address the controller (VXI-11.2 Bus Address).
    pub async fn set_controller_pad(&mut self, pad: u8) -> Result<()> {
        self.ctrl.set_controller_pad(pad).await
    }

    /// Send data with no addressing sequence (IEEE 488.2 16.2.3): the
    /// caller has established addressing itself, via raw bus commands.
    pub async fn send_data_unaddressed(&mut self, data: &[u8], send_eoi: bool) -> Result<()> {
        self.ctrl.send_data_unaddressed(data, send_eoi).await
    }

    /// Receive data with no addressing sequence (IEEE 488.2 16.2.6), capped
    /// at [`MAX_READ`] like the addressed read.
    pub async fn read_unaddressed(&mut self, max_len: usize) -> Result<(Vec<u8>, bool)> {
        self.ctrl.read_unaddressed(max_len.min(MAX_READ)).await
    }
}
