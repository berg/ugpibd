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

pub mod agilent_82357;
pub mod ni_usb_hs;
pub mod select;

use select::UsbSelector;

/// The daemon shares one opened adapter across both front-ends behind this.
pub type SharedBackend = Arc<Mutex<dyn GpibBackend>>;

/// A live read of the eight GPIB control lines.
///
/// Level, not edge: this answers "what is the bus doing *right now*". That is
/// what separates diagnoses which otherwise look identical from the data path
/// — an instrument that is silent from one that is talking to somebody else,
/// or a read that returns nothing because nothing was sent from one that
/// returns nothing because another controller holds ATN.
///
/// Both supported chips expose these in a single register with the same bit
/// layout (TMS9914 `BSR`, TNT4882 `BSR`), so the decode is shared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusLines {
    pub ren: bool,
    pub ifc: bool,
    pub srq: bool,
    pub eoi: bool,
    pub nrfd: bool,
    pub ndac: bool,
    pub dav: bool,
    pub atn: bool,
    /// The register byte this was decoded from, so callers can report what was
    /// actually read and not only our interpretation of it.
    pub raw: u8,
}

impl BusLines {
    pub const REN: u8 = 0x01;
    pub const IFC: u8 = 0x02;
    pub const SRQ: u8 = 0x04;
    pub const EOI: u8 = 0x08;
    pub const NRFD: u8 = 0x10;
    pub const NDAC: u8 = 0x20;
    pub const DAV: u8 = 0x40;
    pub const ATN: u8 = 0x80;

    pub fn from_bsr(raw: u8) -> Self {
        Self {
            ren: raw & Self::REN != 0,
            ifc: raw & Self::IFC != 0,
            srq: raw & Self::SRQ != 0,
            eoi: raw & Self::EOI != 0,
            nrfd: raw & Self::NRFD != 0,
            ndac: raw & Self::NDAC != 0,
            dav: raw & Self::DAV != 0,
            atn: raw & Self::ATN != 0,
            raw,
        }
    }
}

impl std::fmt::Display for BusLines {
    /// `0x29 REN EOI NDAC` — the raw byte first, then the asserted lines.
    ///
    /// The raw byte is not redundant: if the bit order is ever wrong on some
    /// adapter, a reading that names the wrong lines is indistinguishable from
    /// a strange bus unless the byte it came from is also on screen.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#04x}", self.raw)?;
        for (on, name) in [
            (self.atn, "ATN"),
            (self.dav, "DAV"),
            (self.ndac, "NDAC"),
            (self.nrfd, "NRFD"),
            (self.eoi, "EOI"),
            (self.srq, "SRQ"),
            (self.ifc, "IFC"),
            (self.ren, "REN"),
        ] {
            if on {
                write!(f, " {name}")?;
            }
        }
        if self.raw == 0 {
            write!(f, " (none asserted)")?;
        }
        Ok(())
    }
}

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

    /// Address the instrument at `pad` as listener, which is what puts it into
    /// remote state while REN is asserted.
    ///
    /// Addressing alone does it — REN gates the transition, the listen address
    /// triggers it — so this is `ren(true)` plus an addressing sequence, and is
    /// how `viGpibControlREN(VI_GPIB_REN_ASSERT_ADDRESS)` differs from a plain
    /// assert.
    async fn go_to_remote(&mut self, pad: u8) -> Result<()>;

    /// Send Go To Local (GTL) to the instrument at `pad`, returning that one
    /// device to front-panel control.
    ///
    /// Addressed, unlike dropping REN, which returns *every* device on the bus
    /// to local. Note the effect is undone by the next write to the device:
    /// addressing it as a listener with REN still asserted puts it straight
    /// back into remote, which is the standard's behaviour and not a bug here.
    async fn go_to_local(&mut self, pad: u8) -> Result<()>;

    /// Send Local Lockout (LLO), disabling the front-panel local key on every
    /// device on the bus.
    ///
    /// Universal, so it takes no address: the standard offers no per-device
    /// lockout. Cleared by dropping REN.
    async fn local_lockout(&mut self) -> Result<()>;

    /// Serial-poll the instrument at `pad` and return its status byte. This
    /// backs both the Prologix `++spoll` and the HiSLIP `get_status` operation.
    ///
    /// There is deliberately no default: a status byte of 0 means "no bits set,
    /// nothing to report", so a backend that silently returned one would be
    /// indistinguishable from a working serial poll and would hang any script
    /// that polls until a bit sets. A backend that cannot serial-poll must say
    /// so, exactly as `srq_asserted` does below.
    async fn serial_poll(&mut self, pad: u8) -> Result<u8>;

    /// Whether the SRQ line is currently asserted by some device on the bus.
    ///
    /// This is a level read of the physical line, not an event: it answers
    /// "is anyone requesting service right now". The default reports that the
    /// backend cannot tell, which callers must surface as an error rather than
    /// as "no SRQ" — a fabricated "no" is indistinguishable from a working bus
    /// and silently breaks any script that polls for service requests.
    async fn srq_asserted(&mut self) -> Result<bool> {
        anyhow::bail!("{} cannot read the SRQ line", self.name())
    }

    /// Read all eight GPIB control lines as they stand right now.
    ///
    /// A superset of `srq_asserted`, and the tool for telling apart failures
    /// that look identical from the data path: whether a read returned nothing
    /// because nothing was sent, or because another controller holds ATN and
    /// we are no longer in charge.
    ///
    /// The default refuses for the same reason `srq_asserted` does — a
    /// fabricated all-clear reading is indistinguishable from a real one, and
    /// would mislead exactly the person trying to diagnose a bus.
    async fn bus_lines(&mut self) -> Result<BusLines> {
        anyhow::bail!("{} cannot read the GPIB control lines", self.name())
    }

    /// Enter or leave unaddressed-listen ("listen only") mode.
    ///
    /// Two things change together, and both are required — the second is the
    /// one that is easy to miss:
    ///
    /// 1. The chip becomes an **unaddressed listener**, accepting every data
    ///    byte on the bus regardless of who is addressed. This is the only way
    ///    to receive from a talk-only source, which by construction has no
    ///    address to point a read at (`docs/CAPTURE.md` §14.2).
    /// 2. The **RFD holdoff is released**. Normal operation holds NRFD between
    ///    reads so bytes are not dropped, but that presents a listener which is
    ///    never ready, and a talk-only talker will refuse to transmit to it —
    ///    an HP 53310A reports "no ready listeners?" (§4.7). While capturing we
    ///    must be continuously ready, which means giving up that flow control.
    ///
    /// Because of (2) this is **mutually exclusive with ordinary controller
    /// traffic**: with the holdoff gone, bytes can arrive between reads with
    /// nowhere to go. Callers must refuse addressed operations while it is on
    /// rather than let them silently corrupt a capture.
    ///
    /// Runtime-switchable by design: both halves are register writes needing no
    /// re-initialisation, so this is a mode the daemon enters and leaves, not a
    /// mode it must be started in.
    async fn set_listen_only(&mut self, enable: bool) -> Result<()> {
        let _ = enable;
        anyhow::bail!("{} cannot enter listen-only mode", self.name())
    }

    /// Whether unaddressed-listen is currently on.
    fn listen_only(&self) -> bool {
        false
    }

    /// Become an addressable *device* at `address`, or return to controller.
    ///
    /// This is not listen-only. In listen-only we are still controller and
    /// simply accept every byte; here we stop being a controller at all —
    /// system control is released, REN and IFC are dropped, and we sit at a
    /// primary address waiting for somebody else to address us.
    ///
    /// It is what an instrument that drives its own plot transfer needs. An
    /// SR620 with a plotter address configured emits **nothing at all** until a
    /// device exists at that address: sampled continuously while PRINT was
    /// pressed, not one bus line moved (`docs/CAPTURE.md` §14.17). Listen-only
    /// cannot help there, because there is no traffic to listen to.
    ///
    /// Note what this costs: we are no longer controller-in-charge, so the
    /// "pulse IFC to recover the bus" escape hatch does not apply while it is
    /// on. Returning to controller mode re-initialises the adapter.
    async fn set_device_mode(&mut self, address: Option<u8>) -> Result<()> {
        let _ = address;
        anyhow::bail!("{} cannot act as a GPIB device", self.name())
    }

    /// The address we are answering to as a device, if any.
    fn device_address(&self) -> Option<u8> {
        None
    }

    /// Subscribe to service-request notifications, for adapters that can report
    /// SRQ asynchronously. This is what lets a front-end *push* a service
    /// request to a client instead of making it poll.
    ///
    /// `None` means this backend has no notification path — distinct from
    /// "subscribed, and no SRQ has happened". Callers must not present it as
    /// the latter.
    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        None
    }

    /// Configure the end-of-string terminator used when reading.
    fn set_eos(&mut self, eos_char: u8, enabled: bool);

    /// Set the per-operation GPIB timeout in milliseconds.
    fn set_timeout(&mut self, timeout_ms: u32);

    /// Stable identifier for this adapter kind (e.g. `"agilent-82357b"`).
    fn name(&self) -> &'static str;

    /// Leave the adapter in a clean state before the daemon exits.
    ///
    /// Adapters keep their state across host process restarts, so skipping this
    /// can leave hardware that the next session cannot talk to. Best-effort:
    /// failures are logged, not propagated. The default does nothing.
    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The set of adapter kinds this build knows how to drive. Each variant maps to
/// a submodule providing its id, USB VID/PID table, and `open()` constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Agilent82357b,
    Agilent82357a,
    NiUsbHs,
}

impl BackendKind {
    /// Every known backend, in preference order for auto-detection.
    pub const ALL: &'static [BackendKind] = &[
        BackendKind::Agilent82357b,
        BackendKind::Agilent82357a,
        BackendKind::NiUsbHs,
    ];

    /// The 82357-family model descriptor backing an Agilent variant.
    fn agilent_model(self) -> &'static agilent_82357::Model {
        match self {
            BackendKind::Agilent82357b => &agilent_82357::MODEL_82357B,
            BackendKind::Agilent82357a => &agilent_82357::MODEL_82357A,
            BackendKind::NiUsbHs => unreachable!("not an Agilent model"),
        }
    }

    /// Stable `--backend` identifier.
    pub fn id(self) -> &'static str {
        match self {
            BackendKind::Agilent82357b | BackendKind::Agilent82357a => self.agilent_model().id,
            BackendKind::NiUsbHs => ni_usb_hs::ID,
        }
    }

    /// Human-readable description for `--backend list`.
    pub fn description(self) -> &'static str {
        match self {
            BackendKind::Agilent82357b | BackendKind::Agilent82357a => {
                self.agilent_model().description
            }
            BackendKind::NiUsbHs => ni_usb_hs::DESCRIPTION,
        }
    }

    /// (VID, PID) pairs whose presence indicates this adapter.
    pub fn usb_ids(self) -> &'static [(u16, u16)] {
        match self {
            BackendKind::Agilent82357b | BackendKind::Agilent82357a => self.agilent_model().usb_ids,
            BackendKind::NiUsbHs => ni_usb_hs::USB_IDS,
        }
    }

    /// Whether `pid` is this adapter's *pre-firmware* product id — the id it
    /// enumerates with before its firmware has been uploaded.
    ///
    /// An un-programmed 82357 has no string descriptors of its own, so whatever
    /// product/serial the OS reports for it in that state belongs to some other
    /// device (in practice the parent hub) and must not be shown as the
    /// adapter's. Adapters with no firmware-upload step are never "pre-init".
    pub fn is_preinit_pid(self, pid: u16) -> bool {
        match self {
            BackendKind::Agilent82357b | BackendKind::Agilent82357a => {
                self.agilent_model().pid_preinit == pid
            }
            BackendKind::NiUsbHs => false,
        }
    }

    /// Resolve a `--backend` id string to a kind.
    pub fn from_id(id: &str) -> Option<BackendKind> {
        BackendKind::ALL.iter().copied().find(|k| k.id() == id)
    }

    /// Open, initialize, and return the adapter ready for use. `port` restricts
    /// the search to the device at that USB port id (see `select::port_id`).
    pub async fn open(self, timeout_ms: u32, port: Option<&str>) -> Result<SharedBackend> {
        match self {
            BackendKind::Agilent82357b | BackendKind::Agilent82357a => {
                agilent_82357::open(self.agilent_model(), timeout_ms, port).await
            }
            BackendKind::NiUsbHs => ni_usb_hs::open(timeout_ms, port).await,
        }
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

/// Open a backend. `backend` is `None` to accept any kind, or `Some(id)` to
/// require a specific one; `selector` picks among several attached adapters by
/// USB port. Errors (naming the candidates) unless exactly one adapter matches.
///
/// Returns the opened adapter and the USB port id it is bound to, so the caller
/// can watch for that adapter being unplugged.
pub async fn open_selected(
    selector: &UsbSelector,
    backend: Option<&str>,
    timeout_ms: u32,
) -> Result<(SharedBackend, String)> {
    if let Some(id) = backend {
        BackendKind::from_id(id)
            .with_context(|| format!("unknown backend {id:?} (known: {})", known_ids()))?;
    }
    let found = select::enumerate()?;
    let chosen = select::resolve(&found, backend, selector)?;
    info!(
        "using backend {} at USB port {}",
        chosen.kind.id(),
        chosen.port_id
    );
    let port_id = chosen.port_id.clone();
    let opened = chosen.kind.open(timeout_ms, Some(&port_id)).await?;
    Ok((opened, port_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agilent_82357::protocol::{
        USB_PID_82357A, USB_PID_82357A_PREINIT, USB_PID_82357B, USB_PID_82357B_PREINIT,
    };

    #[test]
    fn bus_lines_decode_each_bit_in_isolation() {
        for (bit, get) in [
            (
                BusLines::REN,
                (|l: &BusLines| l.ren) as fn(&BusLines) -> bool,
            ),
            (BusLines::IFC, |l| l.ifc),
            (BusLines::SRQ, |l| l.srq),
            (BusLines::EOI, |l| l.eoi),
            (BusLines::NRFD, |l| l.nrfd),
            (BusLines::NDAC, |l| l.ndac),
            (BusLines::DAV, |l| l.dav),
            (BusLines::ATN, |l| l.atn),
        ] {
            let lines = BusLines::from_bsr(bit);
            assert!(get(&lines), "bit {bit:#04x} did not decode to its own line");
            // and nothing else came along with it
            assert_eq!(lines.raw, bit);
            let others = [
                lines.ren, lines.ifc, lines.srq, lines.eoi, lines.nrfd, lines.ndac, lines.dav,
                lines.atn,
            ]
            .iter()
            .filter(|b| **b)
            .count();
            assert_eq!(others, 1, "bit {bit:#04x} decoded to more than one line");
        }
    }

    /// The bit order in `agilent_82357/protocol.rs` was established on hardware,
    /// not from a datasheet: an idle bus read `0x29` and the same bus with an
    /// instrument asserting SRQ read `0x2d`. Pin both, so a future reshuffle of
    /// the constants has to explain itself against a real measurement.
    #[test]
    fn bus_lines_match_the_readings_the_bit_order_was_derived_from() {
        let idle = BusLines::from_bsr(0x29);
        assert!(idle.ren && idle.eoi && idle.ndac);
        assert!(!idle.srq && !idle.atn && !idle.dav && !idle.ifc && !idle.nrfd);

        let with_srq = BusLines::from_bsr(0x2d);
        assert!(with_srq.srq, "0x2d is 0x29 plus SRQ");
        assert!(with_srq.ren && with_srq.eoi && with_srq.ndac);
    }

    #[test]
    fn bus_lines_display_leads_with_the_raw_byte() {
        assert_eq!(BusLines::from_bsr(0x29).to_string(), "0x29 NDAC EOI REN");
        assert_eq!(BusLines::from_bsr(0x00).to_string(), "0x00 (none asserted)");
        assert_eq!(BusLines::from_bsr(0x80).to_string(), "0x80 ATN");
    }

    #[test]
    fn preinit_pid_recognized_per_model() {
        assert!(BackendKind::Agilent82357a.is_preinit_pid(USB_PID_82357A_PREINIT));
        assert!(BackendKind::Agilent82357b.is_preinit_pid(USB_PID_82357B_PREINIT));
    }

    #[test]
    fn firmware_loaded_pid_is_not_preinit() {
        assert!(!BackendKind::Agilent82357a.is_preinit_pid(USB_PID_82357A));
        assert!(!BackendKind::Agilent82357b.is_preinit_pid(USB_PID_82357B));
    }

    #[test]
    fn models_do_not_claim_each_others_preinit_pid() {
        assert!(!BackendKind::Agilent82357a.is_preinit_pid(USB_PID_82357B_PREINIT));
        assert!(!BackendKind::Agilent82357b.is_preinit_pid(USB_PID_82357A_PREINIT));
    }

    #[test]
    fn adapters_without_firmware_upload_are_never_preinit() {
        for (_, pid) in BackendKind::NiUsbHs.usb_ids() {
            assert!(!BackendKind::NiUsbHs.is_preinit_pid(*pid));
        }
    }
}
