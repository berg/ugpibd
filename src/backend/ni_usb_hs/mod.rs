// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// National Instruments GPIB-USB-HS backend, translated from the Linux kernel
// `drivers/gpib/ni_usb/ni_usb_gpib.c`. Unlike the 82357B this adapter needs no
// firmware upload — it boots ready and performs a control-endpoint readiness
// handshake instead.
//
// Brought up and verified on a physical GPIB-USB-HS (PID 0x709b) against an
// SR620, and on a GPIB-USB-HS+ (0x7618) against an HP 34401A and 53132A.
//
// The HS+ differs in three ways, all handled here: different endpoints (bulk
// 0x01/0x82, interrupt 0x83), a second "analyzer" USB interface we do not
// claim, and the extra bring-up the kernel driver calls
// `ni_usb_hs_plus_extra_init`. It needed no other special casing.
//
// The KUSB-488A and MC-USB-488 share this code path and remain untested, but
// the kernel driver treats them as byte-identical to the plain HS.

pub mod protocol;
pub mod usb;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::backend::{GpibBackend, SharedBackend};
use protocol::*;

/// Backend identifier used by `--backend`.
pub const ID: &str = "ni-usb-hs";

/// Human-readable description shown by `--backend list`.
pub const DESCRIPTION: &str = "NI GPIB-USB-HS and HS+ (KUSB-488A / MC-USB-488 untested)";

/// (VID, PID) pairs handled as GPIB-USB-HS-compatible. The HS+ exposes a second
/// analyzer interface we ignore; KUSB-488A and MC-USB-488 are HS clones.
pub const USB_IDS: &[(u16, u16)] = &[
    (usb::USB_VENDOR_ID_NI, usb::PID_NI_USB_HS),
    (usb::USB_VENDOR_ID_NI, usb::PID_NI_USB_HS_PLUS),
    (usb::USB_VENDOR_ID_NI, usb::PID_KUSB_488A),
    (usb::USB_VENDOR_ID_NI, usb::PID_MC_USB_488),
];

/// Fixed response sizes the adapter returns (see the C reference).
const REG_WRITE_RESP_LEN: usize = 16;
const OP_RESP_LEN: usize = 12;
const REG_READ_RESP_LEN: usize = 32;

/// The low-level USB operations the backend needs. Abstracted so the composite
/// GPIB sequencing can be unit-tested against a mock without hardware.
#[async_trait::async_trait]
pub trait NiTransport: Send + Sync {
    async fn bulk_out(&self, data: &[u8]) -> Result<()>;
    async fn bulk_in(&self, max_len: usize) -> Result<Vec<u8>>;
    async fn control_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        max_len: usize,
    ) -> Result<Vec<u8>>;

    /// Vendor control-IN addressed to the *interface* rather than the device.
    /// Only the HS+ extra-init sequence needs this recipient.
    async fn control_in_interface(
        &self,
        request: u8,
        value: u16,
        index: u16,
        max_len: usize,
    ) -> Result<Vec<u8>>;

    /// USB product id, used to select the model-specific bring-up steps.
    fn product_id(&self) -> u16;

    /// Send a request and read its reply as one indivisible exchange.
    ///
    /// Every adapter operation is a bulk-out followed by the bulk-in carrying
    /// its status, and the two must not be separated: anything else touching
    /// the adapter in between leaves the reply to be read by the next request,
    /// and every reply from then on answers the one before. Real transports
    /// override this to hold their I/O lock across the pair; the default is the
    /// naive version, which is all a single-threaded mock needs.
    async fn transact(&self, req: &[u8], resp_len: usize) -> Result<Vec<u8>> {
        self.bulk_out(req).await?;
        self.bulk_in(resp_len).await
    }

    /// Receiver for service-request notifications, when the transport reads the
    /// adapter's interrupt endpoint. `None` means it cannot observe SRQ, which
    /// callers must treat as "unknown", never as "no SRQ".
    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        None
    }

    /// Discard any responses the adapter still owes from a previous session.
    ///
    /// The bulk pipe is strictly request/response, so one stale reply
    /// desynchronises every transaction after it: the symptom is `missing chunk
    /// start id` on this adapter and `unexpected response byte` on the 82357.
    /// It happens whenever a host dies with a transfer in flight, which made a
    /// fresh daemon fail its first init and succeed on the second — the retry
    /// "worked" only because it consumed the leftovers.
    ///
    /// Defaulted to a no-op: this is a property of a real USB pipe, and a mock
    /// transport with pre-queued responses would have them eaten.
    async fn drain_stale_responses(&self) {}

    /// Re-arm the adapter's interrupt monitor for `mask`.
    ///
    /// Separate from `control_in` because it must be serialised against whole
    /// bulk transactions, which only the transport can do. The default is a
    /// no-op for transports that cannot report SRQ.
    async fn rearm_srq(&self, _mask: u16) -> Result<()> {
        Ok(())
    }
}

/// GPIB-USB-HS controller. The controller's own primary address is fixed at 0;
/// `pad` arguments address the remote instrument.
pub struct NiUsbHsBackend<T: NiTransport> {
    transport: T,
    my_pad: u8,
    listen_only: bool,
    device_address: Option<u8>,
    eos_char: u8,
    eos_enabled: bool,
    timeout_ms: u32,
}

impl<T: NiTransport> NiUsbHsBackend<T> {
    pub fn new(transport: T, timeout_ms: u32) -> Self {
        Self {
            transport,
            my_pad: 0,
            listen_only: false,
            device_address: None,
            eos_char: b'\n',
            eos_enabled: false,
            timeout_ms,
        }
    }

    fn eos_mode(&self) -> u16 {
        // REOS (terminate read on eos char). Exact NI eos bits are unverified;
        // kept minimal since the daemon does not enable EOS at runtime today.
        if self.eos_enabled {
            0x0400
        } else {
            0
        }
    }

    /// Send a bulk request and read a fixed-size response, surfacing transport
    /// errors. The response is returned for the caller to parse.
    async fn transact(&self, req: &[u8], resp_len: usize) -> Result<Vec<u8>> {
        self.transport.transact(req, resp_len).await
    }

    /// Take control (assert ATN), send command bytes, then optionally return to
    /// standby. Command bytes are capped at 16 per transfer by the hardware.
    async fn send_command(&self, cmd: &[u8], standby_after: bool) -> Result<()> {
        self.send_command_bounded(cmd, standby_after, self.timeout_ms)
            .await
    }

    /// `send_command` with an explicit adapter-side timeout, for callers that
    /// expect the transfer may legitimately find no acceptor (init on an empty
    /// bus) and must not stall for the full bus timeout finding out.
    async fn send_command_bounded(
        &self,
        cmd: &[u8],
        standby_after: bool,
        timeout_ms: u32,
    ) -> Result<()> {
        let tc = timeout_code(timeout_ms);
        self.transact(&encode_take_control(true), OP_RESP_LEN)
            .await?;
        for chunk in cmd.chunks(16) {
            let resp = self
                .transact(&encode_command(chunk, tc), OP_RESP_LEN)
                .await?;
            parse_write_response(&resp, chunk.len()).context("ni command byte transfer")?;
        }
        if standby_after {
            self.transact(&encode_go_to_standby(), OP_RESP_LEN).await?;
        }
        Ok(())
    }

    /// Read back `regs` as (device, address) pairs, in order.
    async fn register_read(&self, regs: &[(u8, u8)]) -> Result<Vec<u8>> {
        let resp = self
            .transact(&encode_register_read(regs), REG_READ_RESP_LEN)
            .await?;
        parse_register_read_response(&resp, regs.len())
    }

    async fn register_write(&self, regs: &[NiRegister]) -> Result<()> {
        let resp = self
            .transact(&encode_register_write(regs), REG_WRITE_RESP_LEN)
            .await?;
        let (_status, completed) = parse_register_write_response(&resp)?;
        if completed as usize != regs.len() {
            anyhow::bail!("ni register write: {completed} of {} completed", regs.len());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl<T: NiTransport + 'static> GpibBackend for NiUsbHsBackend<T> {
    async fn init(&mut self, my_pad: u8) -> Result<()> {
        self.my_pad = my_pad;
        // Clear anything left queued by a dead predecessor before trusting a
        // single response byte.
        self.transport.drain_stale_responses().await;
        // Readiness handshake: the adapter needs a moment after enumeration.
        usb::wait_for_ready(&self.transport).await?;
        // Monitor nothing while the chip is being reconfigured, per the kernel
        // driver's attach ordering.
        //
        // The HS+ wants three more vendor reads before it will talk GPIB.
        if self.transport.product_id() == usb::PID_NI_USB_HS_PLUS {
            usb::hs_plus_extra_init(&self.transport).await?;
        }
        let regs = setup_init(my_pad, self.eos_mode());
        self.register_write(&regs)
            .await
            .context("ni init register sequence")?;

        // The register sequence makes us system controller but leaves us off
        // the bus: CIC is still clear, so asserting ATN (take-control) and any
        // command-byte transfer are rejected. Pulsing IFC is what actually
        // makes this adapter Controller-In-Charge. Assert REN too, so
        // instruments accept remote programming — this mirrors the 82357B
        // backend's init.
        self.ifc().await.context("ni init: interface clear")?;
        self.ren(true).await.context("ni init: assert REN")?;

        // Make ourselves the addressed talker before any client traffic.
        //
        // A freshly plugged adapter comes up without TACS, and its first
        // transfer then fails: the addressing is reported as sent, and the data
        // write that follows times out having placed zero bytes, because
        // nothing was ever really addressed to listen. The tell is the
        // take-control status — a cold adapter reports CIC+ATN (0x30) where one
        // that has been used reports CIC+ATN+TACS (0x38). It is invisible on a
        // warm adapter because this hardware keeps its GPIB state across host
        // restarts, so only the first session after a replug pays for it, and
        // only for its first transfer.
        //
        // UNL leaves nobody addressed to listen and MTA makes us the talker, so
        // this changes nothing on the bus beyond leaving it idle — which is
        // where init should leave it anyway — while establishing the state whose
        // absence causes the failure. Untalking here instead would be worse
        // than useless: it clears the very bit we need.
        // Non-fatal, and on a short leash. Command bytes are handshaken like
        // any others, so with no powered instrument on the bus there is no
        // acceptor and this transfer cannot complete — and an empty bus is a
        // legitimate state to start in, not an error (ROADMAP gap 6: failing
        // here blocked first use, where plugging in the adapter before the
        // instruments is the obvious order). Both in-tree kernel drivers make
        // the same call with `skip_check_for_command_acceptors = 1`. The short
        // timeout keeps an empty-bus startup quick; with an acceptor present
        // the bytes complete in microseconds, so it cannot misfire. The cost
        // when this is skipped is only the cold-adapter quirk returning for
        // one transfer once instruments do appear.
        if let Err(e) = self
            .send_command_bounded(&[GPIB_UNL, talk_address(my_pad)], false, 100)
            .await
        {
            warn!("ni init: addressing found no acceptor (empty bus?), continuing: {e:#}");
        }

        // Spend the first data write here, because a freshly plugged adapter
        // loses it. Its command path is fine — addressing is accepted and
        // reported as sent — but the first NIUSB_DATA_WRITE_OP after a replug
        // times out having placed zero bytes, and every one after it succeeds.
        // The adapter reports identical status throughout (CIC/TACS/ATN all as
        // expected), so there is nothing to test for and nothing to wait on;
        // the only way found to clear it is to perform one and discard it.
        //
        // Safe because of the UNL just above: with nothing addressed to listen,
        // this byte is never handshaken by any instrument. Do not reorder it
        // ahead of that command. A short timeout keeps init quick, since with
        // no listener the write is expected to fail.
        let doomed = encode_data_write(&[0x00], false, timeout_code(10));
        if let Err(e) = self.transact(&doomed, OP_RESP_LEN).await {
            debug!("ni init: priming data write did not complete: {e}");
        }

        // Arm the interrupt monitor for service requests, and nothing else.
        //
        // Last, deliberately. Everything above disturbs it: the register
        // sequence resets the chip, and IFC, REN and the priming transfers all
        // move the bus. Arming before them left the monitor cleared, so the
        // only report ever seen was the acknowledgement of the arming itself
        // and no service request was ever delivered.
        //
        // Safe only because the transport reads the interrupt endpoint from the
        // moment it opens: a reported bit nobody reads backs up and stalls the
        // adapter's bulk transfers on the *next* session, recoverable only by
        // replugging. `shutdown` disarms, keeping that promise on a clean exit.
        //
        // SRQ alone, never `IBSTA_MONITOR_MASK`. The adapter reports a monitored
        // bit that is already set straight away, and most of that mask — CIC,
        // TACS, ATN — is true nearly always, so arming with it yields a
        // continuous stream of reports carrying no news.
        usb::set_interrupt_monitor(&self.transport, IBSTA_SRQI).await?;

        info!("NI GPIB-USB-HS initialized at pad {my_pad}");
        Ok(())
    }

    async fn write(&mut self, pad: u8, data: &[u8], send_eoi: bool) -> Result<()> {
        // Refuse rather than corrupt: in listen-only we are not the controller
        // of this transfer and the RFD holdoff is released, so addressing
        // anyone would both fail and disturb a capture in progress.
        if self.listen_only {
            anyhow::bail!("cannot write while in listen-only mode (++lon 0 to leave)");
        }
        if self.device_address.is_some() {
            anyhow::bail!("cannot write while in device mode: we are not the controller");
        }
        // Address controller as talker (pad 0), instrument as listener.
        let cmd = [GPIB_UNL, talk_address(self.my_pad), listen_address(pad)];
        self.send_command(&cmd, true).await?;

        let tc = timeout_code(self.timeout_ms);
        // Split anything past the adapter's 16-bit length field, asserting EOI
        // only on the final chunk so the message still terminates once.
        let mut remaining = data;
        loop {
            let n = remaining.len().min(MAX_TRANSFER_LEN);
            let (chunk, rest) = remaining.split_at(n);
            let last = rest.is_empty();
            let resp = self
                .transact(&encode_data_write(chunk, send_eoi && last, tc), OP_RESP_LEN)
                .await?;
            let written = parse_write_response(&resp, chunk.len()).context("ni data write")?;
            if written != chunk.len() {
                anyhow::bail!(
                    "ni data write: instrument accepted only {written} of {} bytes",
                    chunk.len()
                );
            }
            if last {
                return Ok(());
            }
            remaining = rest;
        }
    }

    async fn read(&mut self, pad: u8, max_len: usize) -> Result<(Vec<u8>, bool)> {
        // Clamp to what the adapter's length field can express. Callers such as
        // the HiSLIP server ask for 64 KiB, one byte past the limit, which would
        // otherwise wrap the encoded count to zero and read nothing at all.
        let max_len = max_len.min(MAX_TRANSFER_LEN);
        if self.device_address.is_some() {
            // We are not the controller. Sending command bytes is not ours to
            // do; just take whatever the controller addresses us to receive.
            let req = encode_data_read(
                max_len,
                self.eos_mode(),
                self.eos_char,
                timeout_code(self.timeout_ms),
            );
            let resp_cap = (max_len / 30 + 1) * 0x20 + 0x20;
            let resp = self.transact(&req, resp_cap).await?;
            return parse_data_read_response(&resp, max_len).context("ni device read");
        }
        // Address ourselves as sole listener in listen-only, naming no talker:
        // a talk-only source has no address (31 is the untalk code) and is
        // already talking. This per-read addressing is what was measured
        // working — 558 rows, complete (`docs/CAPTURE.md` §14.15). The 82357
        // needs the addressing hoisted out of the read instead, because it
        // asserts ATN differently; do not "unify" these without re-measuring
        // both.
        let cmd: &[u8] = if self.listen_only {
            &[GPIB_UNL, listen_address(self.my_pad)]
        } else {
            &[GPIB_UNL, listen_address(self.my_pad), talk_address(pad)]
        };
        self.send_command(cmd, true).await?;
        let req = encode_data_read(
            max_len,
            self.eos_mode(),
            self.eos_char,
            timeout_code(self.timeout_ms),
        );
        // Data comes back in 15/30-byte framed blocks plus two status blocks.
        let resp_cap = (max_len / 30 + 1) * 0x20 + 0x20;
        let resp = self.transact(&req, resp_cap).await?;
        parse_data_read_response(&resp, max_len).context("ni data read")
    }

    async fn device_clear(&mut self, pad: u8) -> Result<()> {
        let cmd = [GPIB_UNL, listen_address(pad), GPIB_SDC];
        self.send_command(&cmd, false).await
    }

    async fn go_to_remote(&mut self, pad: u8) -> Result<()> {
        self.ren(true).await?;
        // Addressing as listener is the transition; REN only permits it.
        let cmd = [GPIB_UNL, listen_address(pad)];
        self.send_command(&cmd, false).await
    }

    async fn go_to_local(&mut self, pad: u8) -> Result<()> {
        let cmd = [GPIB_UNL, listen_address(pad), GPIB_GTL];
        self.send_command(&cmd, false).await
    }

    async fn local_lockout(&mut self) -> Result<()> {
        // Universal command: no addressing, every device obeys it.
        self.send_command(&[GPIB_LLO], false).await
    }

    async fn trigger(&mut self, pad: u8) -> Result<()> {
        let cmd = [GPIB_UNL, listen_address(pad), GPIB_GET];
        self.send_command(&cmd, false).await
    }

    async fn ifc(&mut self) -> Result<()> {
        // IFC is a single pulse; the adapter has no separate de-assert.
        self.transact(&encode_interface_clear(), OP_RESP_LEN)
            .await?;
        Ok(())
    }

    async fn ren(&mut self, enable: bool) -> Result<()> {
        let aux = if enable { AUX_SREN } else { AUX_CREN };
        self.register_write(&[NiRegister::new(SUBDEV_TNT4882, REG_AUXMR, aux)])
            .await
    }

    async fn serial_poll(&mut self, pad: u8) -> Result<u8> {
        // SPE, address instrument as talker + controller as listener, standby,
        // read one status byte, then SPD/UNT.
        let enable = [
            GPIB_UNL,
            GPIB_SPE,
            talk_address(pad),
            listen_address(self.my_pad),
        ];
        self.send_command(&enable, true).await?;
        let req = encode_data_read(1, 0, 0, timeout_code(self.timeout_ms));
        let resp = self.transact(&req, 0x40).await?;
        let (data, _end) = parse_data_read_response(&resp, 1)?;
        self.send_command(&[GPIB_SPD, GPIB_UNT], false).await?;

        // Re-arm here, because this is exactly the point where a service
        // request has been dealt with. The adapter's monitor is one-shot per
        // bit — the kernel clears each reported bit from the set it waits on —
        // so without this the first report is the only one ever delivered.
        // Re-arming from the reader task instead would race: it is a control
        // transfer, and one landing between a bulk-out and its bulk-in
        // desynchronises every reply that follows.
        if let Err(e) = self.transport.rearm_srq(IBSTA_SRQI).await {
            debug!("ni: re-arming the srq monitor failed: {e:#}");
        }
        Ok(data.first().copied().unwrap_or(0))
    }

    async fn send_bus_command(&mut self, cmds: &[u8]) -> Result<()> {
        // Take control, put the bytes on the bus under ATN, drop to standby:
        // the same sequence every addressed operation here already uses.
        self.send_command(cmds, true).await
    }

    async fn set_atn(&mut self, assert: bool) -> Result<()> {
        let req = if assert {
            encode_take_control(true)
        } else {
            encode_go_to_standby()
        };
        self.transact(&req, OP_RESP_LEN).await.map(|_| ())
    }

    fn controller_pad(&self) -> u8 {
        self.my_pad
    }

    /// Read the TNT4882 bus status register and report the live SRQ line.
    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        self.transport.subscribe_srq()
    }

    async fn srq_asserted(&mut self) -> Result<bool> {
        Ok(self.bus_lines().await?.srq)
    }

    async fn set_listen_only(&mut self, enable: bool) -> Result<()> {
        // Preserve the binary-EOS bit the init sequence computes; it lives in
        // the same AUXRA write as the holdoff bit, so rewriting one without it
        // would silently turn binary mode off.
        let bin = if self.eos_mode() & 0x0400 != 0 {
            HR_BIN
        } else {
            0
        };
        let regs = if enable {
            vec![
                // Unaddressed listener.
                NiRegister::new(SUBDEV_TNT4882, REG_ADMR, ADMR_DISABLE_SAD | HR_LON),
                // Holdoff mode off, *then* finish any handshake already held
                // off. Clearing the mode alone leaves an asserted holdoff
                // asserted, and the talker stays blocked on the byte we are
                // already sitting on.
                //
                // AUXMR only. `setup_init` also writes the AUXRA value to
                // REG_AUXCR, but that is an init-context alias: REG_AUXCR and
                // REG_SPMR are both offset 0x06, so outside init the same write
                // lands in the serial-poll mode register and sets RSV — making
                // the adapter assert SRQ. Mirroring it here broke capture until
                // it was removed.
                NiRegister::new(SUBDEV_TNT4882, REG_AUXMR, AUXRA | bin),
                NiRegister::new(SUBDEV_TNT4882, REG_AUXMR, AUX_FH),
            ]
        } else {
            // Leaving is not the inverse of entering. Clearing HR_LON and
            // restoring the holdoff bit leaves the chip still holding NDAC and
            // NRFD, and it can no longer address anyone — measured on a
            // GPIB-USB-HS+, where an *IDN? after ++lon 0 failed with "no
            // listener on bus" and the lines stayed at 0x31.
            //
            // So leave by re-initialising to a known-good controller state
            // rather than by hand-crafting an inverse of a sequence whose
            // TNT4882 semantics are not documented here. This pulses IFC,
            // which is acceptable and arguably correct: taking the bus back is
            // exactly what leaving capture mode means.
            self.listen_only = false;
            let my_pad = self.my_pad;
            self.init(my_pad).await.context("ni leave listen-only")?;
            tracing::info!("left listen-only; controller re-initialised");
            return Ok(());
        };
        self.register_write(&regs)
            .await
            .context("ni enter listen-only")?;
        self.listen_only = enable;
        tracing::info!(listen_only = enable, "unaddressed listen");
        Ok(())
    }

    fn listen_only(&self) -> bool {
        self.listen_only
    }

    async fn set_device_mode(&mut self, address: Option<u8>) -> Result<()> {
        let Some(addr) = address else {
            // Back to controller. Re-initialise rather than trying to invert
            // the sequence below: `init` retakes system control and pulses IFC,
            // which is exactly what reclaiming the bus means.
            self.device_address = None;
            let my_pad = self.my_pad;
            self.init(my_pad).await.context("ni leave device mode")?;
            tracing::info!("left device mode; controller re-initialised");
            return Ok(());
        };
        if addr > 30 {
            anyhow::bail!("device address {addr} is out of range (0-30)");
        }
        // Release system control, in the order the kernel driver uses
        // (`ni_usb_gpib.c:1087-1103`): drop REN, drop IFC, disable system
        // control, then clear the system-controller bit. Dropping REN first
        // matters — it is what returns every instrument on the bus to local,
        // and we are about to stop being the controller that asserted it.
        let regs = vec![
            NiRegister::new(SUBDEV_TNT4882, REG_ADR, addr & 0x1f),
            NiRegister::new(SUBDEV_TNT4882, REG_AUXMR, AUX_CREN),
            NiRegister::new(SUBDEV_TNT4882, REG_AUXMR, AUX_CIFC),
            NiRegister::new(SUBDEV_TNT4882, REG_AUXMR, AUX_DSC),
            NiRegister::new(SUBDEV_TNT4882, REG_CMDR, CMDR_CLRSC),
        ];
        self.register_write(&regs)
            .await
            .context("ni enter device mode")?;
        self.device_address = Some(addr);
        self.listen_only = false;
        tracing::info!(address = addr, "device mode: no longer controller");
        Ok(())
    }

    fn device_address(&self) -> Option<u8> {
        self.device_address
    }

    async fn bus_lines(&mut self) -> Result<crate::backend::BusLines> {
        let vals = self
            .register_read(&[(SUBDEV_TNT4882, REG_BSR)])
            .await
            .context("ni bus status read")?;
        let bsr = *vals
            .first()
            .ok_or_else(|| anyhow::anyhow!("ni bus status read returned no data"))?;
        let lines = crate::backend::BusLines::from_bsr(bsr);
        tracing::debug!(bus = %lines, "bus status");
        Ok(lines)
    }

    fn set_eos(&mut self, eos_char: u8, enabled: bool) {
        self.eos_char = eos_char;
        self.eos_enabled = enabled;
    }

    fn eos(&self) -> (u8, bool) {
        (self.eos_char, self.eos_enabled)
    }

    fn set_timeout(&mut self, timeout_ms: u32) {
        self.timeout_ms = timeout_ms;
    }

    fn name(&self) -> &'static str {
        ID
    }

    /// Mirror the kernel driver's detach: stop interrupt monitoring, reset the
    /// TNT4882, then write 0 to device 3 register 0x10 — the same "software
    /// unplug" the Windows driver performs.
    ///
    /// Without this the adapter keeps its GPIB state (addressed, RFD held off,
    /// operations pending) after the daemon exits, and the next session can
    /// find it unresponsive right down to the USB control endpoint.
    async fn shutdown(&mut self) -> Result<()> {
        usb::set_interrupt_monitor(&self.transport, 0).await?;
        self.register_write(&[
            NiRegister::new(SUBDEV_TNT4882, REG_AUXMR, AUX_CR),
            NiRegister::new(SUBDEV_UNKNOWN3, 0x10, 0x00),
        ])
        .await
        .context("ni shutdown register write")?;
        info!("NI GPIB-USB-HS shut down cleanly");
        Ok(())
    }
}

/// Discover, open, and initialize an NI GPIB-USB-HS. `port` restricts the
/// search to the device at that USB port id.
pub async fn open(timeout_ms: u32, port: Option<&str>) -> Result<SharedBackend> {
    // Try a few times: quiesce -> init. The adapter keeps its GPIB state across
    // host process restarts, so a daemon that died mid-transfer can leave a
    // pending operation behind; the stop request plus clearing the endpoints
    // usually shakes that loose. The 82357B backend retries the same way.
    //
    // Deliberately no USB-level device reset here. On macOS that makes this
    // adapter drop off the bus entirely until it is physically replugged, so it
    // turns a recoverable wedge into a dead adapter.
    let mut last_err = None;
    for attempt in 1..=3 {
        let transport = usb::NiUsbTransport::open(timeout_ms, port).await?;
        transport.quiesce().await;
        let mut ctrl = NiUsbHsBackend::new(transport, timeout_ms);

        // The TNT4882 comes out of enumeration unconfigured: until the init
        // register sequence runs it is not system controller and cannot drive
        // ATN, so the first command-byte transfer fails.
        match ctrl.init(0).await {
            Ok(()) => {
                info!("NI adapter initialized (attempt {attempt})");
                return Ok(std::sync::Arc::new(tokio::sync::Mutex::new(ctrl)));
            }
            Err(e) => {
                warn!("ni init attempt {attempt} failed: {e:#}");
                last_err = Some(e);
                drop(ctrl); // release the interface before reopening
                            // Escalate to re-applying the USB configuration, which resets
                            // endpoint state without re-enumerating the device.
                if let Err(e) = usb::reset_configuration(port) {
                    warn!("ni USB configuration reset failed: {e:#}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    Err(last_err.expect("loop runs at least once")).context(
        "ni adapter initialization failed; if it stays stuck, \
         unplug the adapter and plug it back in",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records outgoing bulk packets and replays queued bulk-in responses.
    struct MockTransport {
        written: Mutex<Vec<Vec<u8>>>,
        responses: Mutex<Vec<Vec<u8>>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Vec<u8>>) -> Self {
            Self {
                written: Mutex::new(vec![]),
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait::async_trait]
    impl NiTransport for MockTransport {
        async fn bulk_out(&self, data: &[u8]) -> Result<()> {
            self.written.lock().unwrap().push(data.to_vec());
            Ok(())
        }
        async fn bulk_in(&self, _max_len: usize) -> Result<Vec<u8>> {
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                anyhow::bail!("mock: bulk_in with no response queued");
            }
            Ok(q.remove(0))
        }
        async fn control_in(&self, request: u8, _v: u16, _i: u16, _m: usize) -> Result<Vec<u8>> {
            let mut reply = match request {
                // Echo the request id, then a little-endian serial number.
                0x41 => vec![0x41, 0xb7, 0xde, 0x20, 0x01],
                // Poll-ready block as captured from a real GPIB-USB-HS. Bytes
                // 6, 7 and 10 are non-zero, which is what signals "ready".
                0x40 => vec![0x40, 1, 0, 1, 0x30, 1, 2, 5, 2, 0, 7],
                // Stop / interrupt-monitor return a bare 8-byte status block.
                _ => vec![request, 0, 0, 0, 0, 0, 0, 0],
            };
            if request == 0x41 || request == 0x40 {
                reply.resize(16, 0);
            }
            Ok(reply)
        }
        async fn control_in_interface(
            &self,
            request: u8,
            v: u16,
            i: u16,
            m: usize,
        ) -> Result<Vec<u8>> {
            self.control_in(request, v, i, m).await
        }
        fn product_id(&self) -> u16 {
            usb::PID_NI_USB_HS
        }
    }

    /// The bulk responses `init()` consumes after the readiness handshake:
    /// the 26-register init write, the IFC pulse, and the REN register write.
    fn init_responses() -> Vec<Vec<u8>> {
        vec![
            reg_write_ok(26),
            op_ok(),         // IFC
            reg_write_ok(1), // REN
            op_ok(),         // priming: take control
            op_ok(),         // priming: UNL + MTA
            op_ok(),         // priming: the discarded data write
        ]
    }

    fn reg_write_ok(completed: u8) -> Vec<u8> {
        let mut b = vec![NIUSB_REG_WRITE_ID, 0, 0, 0, 0, 0, 0, 0];
        b.push(completed);
        b.extend_from_slice(&[0; 7]);
        b
    }

    fn op_ok() -> Vec<u8> {
        // 12-byte status response, error 0, count 0.
        vec![0u8; 12]
    }

    fn op_err(code: u8) -> Vec<u8> {
        let mut b = vec![0u8; 12];
        b[3] = code;
        b
    }

    /// An empty bus is a legitimate state to start in, not an error. Command
    /// bytes need an acceptor and a bus with no powered instrument has none,
    /// so init's self-addressing cannot complete there — and failing init for
    /// it blocked first use (plug in the adapter, then wire the instruments).
    /// Individual operations still fail with "no listener", which is correct.
    #[tokio::test]
    async fn init_succeeds_on_an_empty_bus() {
        for code in [
            NIUSB_NO_BUS_ERROR,
            NIUSB_NO_LISTENER_ERROR,
            NIUSB_TIMEOUT_ERROR,
        ] {
            let responses = vec![
                reg_write_ok(26),
                op_ok(),         // IFC
                reg_write_ok(1), // REN
                op_ok(),         // priming: take control
                op_err(code),    // priming: UNL + MTA finds no acceptor
                op_ok(),         // priming: the discarded data write
            ];
            let t = MockTransport::new(responses);
            let mut be = NiUsbHsBackend::new(t, 3000);
            be.init(0)
                .await
                .unwrap_or_else(|e| panic!("init must survive error code {code}: {e:#}"));
        }
    }

    /// A backend that cannot do a thing must say so rather than quietly
    /// succeeding — the roadmap's "no plausible lies" rule. A silent success
    /// here would leave a caller believing it was capturing when it was not.
    #[tokio::test]
    async fn mode_switches_report_their_own_state() {
        let t = MockTransport::new(init_responses());
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.init(0).await.unwrap();
        assert!(!be.listen_only());
        assert_eq!(be.device_address(), None);
    }

    /// 31 is the untalk code, not a primary address. Accepting it would point
    /// the daemon at an address that by construction cannot be addressed.
    #[tokio::test]
    async fn device_mode_rejects_addresses_above_30() {
        let t = MockTransport::new(init_responses());
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.init(0).await.unwrap();
        for bad in [31u8, 32, 255] {
            assert!(
                be.set_device_mode(Some(bad)).await.is_err(),
                "address {bad} should have been refused"
            );
            assert_eq!(
                be.device_address(),
                None,
                "a refused address must not stick"
            );
        }
    }

    /// Writing while capturing cannot work and must not be attempted: in
    /// listen-only the RFD holdoff is released and in device mode we are not
    /// the controller at all. Failing loudly beats corrupting a capture.
    #[tokio::test]
    async fn writes_are_refused_while_capturing() {
        let mut responses = init_responses();
        responses.push(reg_write_ok(3)); // set_listen_only registers
        responses.push(op_ok()); // listen-only addressing
        let t = MockTransport::new(responses);
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.init(0).await.unwrap();
        be.set_listen_only(true).await.unwrap();
        assert!(be.listen_only());
        let err = be.write(5, b"*IDN?", true).await.unwrap_err().to_string();
        assert!(
            err.contains("listen-only"),
            "refusal should name the mode, got: {err}"
        );
    }

    #[tokio::test]
    async fn init_writes_registers_then_pulses_ifc_and_asserts_ren() {
        let t = MockTransport::new(init_responses());
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.init(0).await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        assert_eq!(
            writes.len(),
            6,
            "init: register sequence, IFC, REN, take control, address self, \
             then the discarded data write"
        );

        // MTA is the point: a cold adapter has no TACS, and its first transfer
        // fails without it. Untalking here would clear the very bit this is
        // establishing. UNL keeps the bus idle by leaving nobody listening.
        assert_eq!(
            &writes[4][4..6],
            [GPIB_UNL, talk_address(0)],
            "init must unlisten and address itself as talker, never untalk"
        );

        assert_eq!(writes[0][0], NIUSB_REG_WRITE_ID);
        assert_eq!(writes[0][1], 26, "26 register writes");

        // Pulsing IFC is what makes the adapter Controller-In-Charge; without
        // it every subsequent command-byte transfer is rejected.
        assert_eq!(writes[1], encode_interface_clear(), "IFC pulse");

        assert_eq!(writes[2][0], NIUSB_REG_WRITE_ID);
        assert_eq!(
            &writes[2][3..6],
            &[SUBDEV_TNT4882, REG_AUXMR, AUX_SREN],
            "REN asserted"
        );
    }

    #[tokio::test]
    async fn init_fails_when_adapter_never_reports_ready() {
        struct NeverReady;
        #[async_trait::async_trait]
        impl NiTransport for NeverReady {
            async fn bulk_out(&self, _d: &[u8]) -> Result<()> {
                Ok(())
            }
            async fn bulk_in(&self, _m: usize) -> Result<Vec<u8>> {
                Ok(vec![0u8; 16])
            }
            async fn control_in(&self, _r: u8, _v: u16, _i: u16, _m: usize) -> Result<Vec<u8>> {
                // Well-formed reply with the ready bytes all clear.
                Ok(vec![0u8; 16])
            }
            async fn control_in_interface(
                &self,
                _r: u8,
                _v: u16,
                _i: u16,
                _m: usize,
            ) -> Result<Vec<u8>> {
                Ok(vec![0u8; 16])
            }
            fn product_id(&self) -> u16 {
                usb::PID_NI_USB_HS
            }
        }
        let mut be = NiUsbHsBackend::new(NeverReady, 3000);
        assert!(be.init(0).await.is_err(), "must not proceed when not ready");
    }

    #[tokio::test]
    async fn write_addresses_then_sends_data() {
        // take_control, command, go_to_standby, data write -> 4 responses.
        let t = MockTransport::new(vec![op_ok(), op_ok(), op_ok(), op_ok()]);
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.write(23, b"*IDN?", true).await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        assert_eq!(writes.len(), 4);
        // command packet is the second transfer.
        let cmd = &writes[1];
        assert_eq!(cmd[0], NIUSB_COMMAND_OP);
        assert_eq!(&cmd[4..7], &[GPIB_UNL, talk_address(0), listen_address(23)]);
        // last transfer is the data write with EOI + payload.
        let dw = &writes[3];
        assert_eq!(dw[0], NIUSB_DATA_WRITE_OP);
        assert_eq!(dw[6], 0x08);
        assert_eq!(&dw[8..13], b"*IDN?");
    }

    #[tokio::test]
    async fn read_addresses_then_parses_data() {
        // take_control, command, go_to_standby (op_ok x3), then a data-read resp.
        let mut read_resp = vec![NIUSB_IBRD_DATA_ID];
        let mut payload = b"HP".to_vec();
        payload.resize(15, 0);
        read_resp.extend_from_slice(&payload);
        read_resp.extend_from_slice(&[NIUSB_IBRD_STATUS_ID, 0x20, 0x00, 0, 0, 0, 0, 0]);
        read_resp.push(0x00);
        read_resp.push(0x02);
        let t = MockTransport::new(vec![op_ok(), op_ok(), op_ok(), read_resp]);
        let mut be = NiUsbHsBackend::new(t, 3000);
        let (data, end) = be.read(23, 64).await.unwrap();
        assert_eq!(data, b"HP");
        assert!(end);
        let writes = be.transport.written.lock().unwrap().clone();
        let cmd = &writes[1];
        assert_eq!(&cmd[4..7], &[GPIB_UNL, listen_address(0), talk_address(23)]);
    }

    #[tokio::test]
    async fn go_to_local_is_addressed_to_the_listener() {
        // take control, command bytes.
        let t = MockTransport::new(vec![op_ok(), op_ok()]);
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.go_to_local(23).await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        // GTL is addressed, so it returns this one instrument to its front
        // panel; dropping REN would return the whole bus.
        assert_eq!(&writes[1][4..7], &[GPIB_UNL, listen_address(23), GPIB_GTL]);
    }

    #[tokio::test]
    async fn local_lockout_is_universal_and_unaddressed() {
        let t = MockTransport::new(vec![op_ok(), op_ok()]);
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.local_lockout().await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        // No address: IEEE-488 defines no per-device lockout, and inventing
        // one by addressing first would be a different command.
        assert_eq!(&writes[1][4..5], &[GPIB_LLO]);
    }

    #[tokio::test]
    async fn go_to_remote_asserts_ren_then_addresses() {
        // ren(true) register write, then take control + command.
        let t = MockTransport::new(vec![reg_write_ok(1), op_ok(), op_ok()]);
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.go_to_remote(23).await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        assert_eq!(writes[0][0], NIUSB_REG_WRITE_ID, "REN first");
        // REN only permits remote; being addressed to listen is the actual
        // transition, which is what separates this from a plain assert.
        assert_eq!(&writes[2][4..6], &[GPIB_UNL, listen_address(23)]);
    }

    /// Records every vendor control request, reporting whichever PID it is told.
    struct PidTransport {
        requests: std::sync::Arc<Mutex<Vec<u8>>>,
        pid: u16,
        inner: MockTransport,
    }

    #[async_trait::async_trait]
    impl NiTransport for PidTransport {
        async fn bulk_out(&self, d: &[u8]) -> Result<()> {
            self.inner.bulk_out(d).await
        }
        async fn bulk_in(&self, m: usize) -> Result<Vec<u8>> {
            self.inner.bulk_in(m).await
        }
        async fn control_in(&self, r: u8, v: u16, i: u16, m: usize) -> Result<Vec<u8>> {
            self.requests.lock().unwrap().push(r);
            self.inner.control_in(r, v, i, m).await
        }
        async fn control_in_interface(&self, r: u8, v: u16, i: u16, m: usize) -> Result<Vec<u8>> {
            self.requests.lock().unwrap().push(r);
            self.inner.control_in_interface(r, v, i, m).await
        }
        fn product_id(&self) -> u16 {
            self.pid
        }
    }

    async fn init_requests_for(pid: u16) -> Vec<u8> {
        let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let t = PidTransport {
            requests: requests.clone(),
            pid,
            inner: MockTransport::new(init_responses()),
        };
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.init(0).await.unwrap();
        let r = requests.lock().unwrap().clone();
        r
    }

    #[tokio::test]
    async fn hs_plus_gets_extra_init_and_plain_hs_does_not() {
        // The HS+ needs three more vendor reads (0x48, 0x4b LED, 0xf8) before it
        // will talk GPIB; sending them to a plain HS would be wrong.
        let plus = init_requests_for(usb::PID_NI_USB_HS_PLUS).await;
        for req in [0x48, 0x4b, 0xf8] {
            assert!(
                plus.contains(&req),
                "HS+ init must issue {req:#04x}: {plus:02x?}"
            );
        }

        let hs = init_requests_for(usb::PID_NI_USB_HS).await;
        for req in [0x48, 0x4b, 0xf8] {
            assert!(
                !hs.contains(&req),
                "plain HS must not issue {req:#04x}: {hs:02x?}"
            );
        }

        // The clones share the HS path exactly.
        for pid in [usb::PID_KUSB_488A, usb::PID_MC_USB_488] {
            assert_eq!(init_requests_for(pid).await, hs, "clone {pid:#06x} differs");
        }
    }

    #[test]
    fn endpoints_differ_only_for_hs_plus() {
        // Getting these wrong is silent: transfers just never complete.
        let hs = [usb::PID_NI_USB_HS, usb::PID_KUSB_488A, usb::PID_MC_USB_488];
        for pid in hs {
            assert_eq!(
                usb::endpoints_for_test(pid),
                (0x02, 0x84, 0x81),
                "pid {pid:#06x}"
            );
        }
        assert_eq!(
            usb::endpoints_for_test(usb::PID_NI_USB_HS_PLUS),
            (0x01, 0x82, 0x83)
        );
    }

    #[tokio::test]
    async fn shutdown_resets_chip_and_powers_down() {
        // Mirrors the kernel driver's detach. Skipping it leaves the adapter
        // holding GPIB state that the next session cannot clear.
        let t = MockTransport::new(vec![reg_write_ok(2)]);
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.shutdown().await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        assert_eq!(writes.len(), 1, "shutdown is a single register-write bulk");
        assert_eq!(writes[0][1], 2, "two register writes");
        assert_eq!(
            &writes[0][3..6],
            &[SUBDEV_TNT4882, REG_AUXMR, AUX_CR],
            "TNT4882 chip reset"
        );
        assert_eq!(
            &writes[0][6..9],
            &[SUBDEV_UNKNOWN3, 0x10, 0x00],
            "software-unplug power-down write"
        );
    }

    #[tokio::test]
    async fn init_arms_srq_only_and_shutdown_disarms() {
        // Monitoring is safe only while something reads the interrupt endpoint,
        // which the transport starts when it opens. What this pins is the rest
        // of the bargain. Arming must name SRQ alone: the adapter reports an
        // already-set bit immediately, and CIC/TACS/ATN are set nearly always,
        // so the full mask yields a continuous stream of reports about nothing.
        // And shutdown must disarm, or the next session inherits an adapter
        // reporting to nobody.
        //
        // Arming must also come *last*. Everything else in init disturbs it —
        // the register sequence resets the chip, and IFC, REN and the priming
        // transfers all move the bus — so arming earlier leaves the monitor
        // cleared and the only report ever seen is the acknowledgement of the
        // arming itself. That failed silently: every SRQ test still "passed"
        // by delivering nothing, because the checks that catch a missing
        // delivery are the ones that assert something *arrives*. So record the
        // number of bulk exchanges completed at the moment each arming is
        // issued, and require the first one to come after all of them.
        let monitor_masks = std::sync::Arc::new(Mutex::new(Vec::new()));
        struct Recorder(std::sync::Arc<Mutex<Vec<(u16, usize)>>>, MockTransport);
        #[async_trait::async_trait]
        impl NiTransport for Recorder {
            async fn bulk_out(&self, d: &[u8]) -> Result<()> {
                self.1.bulk_out(d).await
            }
            async fn bulk_in(&self, m: usize) -> Result<Vec<u8>> {
                self.1.bulk_in(m).await
            }
            async fn control_in(&self, r: u8, _v: u16, i: u16, m: usize) -> Result<Vec<u8>> {
                if r == 0x21 {
                    let writes = self.1.written.lock().unwrap().len();
                    self.0.lock().unwrap().push((i, writes));
                }
                self.1.control_in(r, _v, i, m).await
            }
            async fn control_in_interface(
                &self,
                r: u8,
                v: u16,
                i: u16,
                m: usize,
            ) -> Result<Vec<u8>> {
                self.1.control_in_interface(r, v, i, m).await
            }
            fn product_id(&self) -> u16 {
                self.1.product_id()
            }
        }
        let mut responses = init_responses();
        responses.push(reg_write_ok(2)); // shutdown's register write
        let t = Recorder(monitor_masks.clone(), MockTransport::new(responses));
        let mut be = NiUsbHsBackend::new(t, 3000);

        be.init(0).await.unwrap();
        let armed = monitor_masks.lock().unwrap().clone();
        let init_writes = be.transport.1.written.lock().unwrap().len();

        assert_eq!(
            armed.iter().map(|&(mask, _)| mask).collect::<Vec<_>>(),
            vec![IBSTA_SRQI],
            "init must arm for SRQ alone, never the whole monitor mask"
        );
        assert_eq!(
            armed[0].1, init_writes,
            "init must arm last: it armed after {} of {init_writes} bulk exchanges, \
             so the chip reset and bus activity that follow would clear it",
            armed[0].1
        );

        be.shutdown().await.unwrap();
        assert_eq!(
            monitor_masks
                .lock()
                .unwrap()
                .iter()
                .map(|&(mask, _)| mask)
                .collect::<Vec<_>>(),
            vec![IBSTA_SRQI, 0],
            "shutdown must disarm the monitor"
        );
    }

    #[tokio::test]
    async fn ren_writes_aux_command() {
        let t = MockTransport::new(vec![reg_write_ok(1)]);
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.ren(true).await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        assert_eq!(writes[0][0], NIUSB_REG_WRITE_ID);
        assert_eq!(&writes[0][3..6], &[SUBDEV_TNT4882, REG_AUXMR, AUX_SREN]);
    }
}
