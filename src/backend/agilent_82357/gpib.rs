// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use super::protocol::*;
use anyhow::Result;

pub trait Transport {
    fn write_bulk(&self, data: &[u8]) -> impl std::future::Future<Output = Result<()>> + Send;
    fn read_bulk(
        &self,
        max_len: usize,
    ) -> impl std::future::Future<Output = Result<Vec<u8>>> + Send;
    /// Issue a vendor control-IN transfer (bmRequestType = 0xC0).
    fn control_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        max_len: usize,
    ) -> impl std::future::Future<Output = Result<Vec<u8>>> + Send;
    /// Block until the device signals write-complete via the interrupt endpoint.
    /// On a mock transport, this returns immediately.
    fn await_write_complete(&self) -> impl std::future::Future<Output = Result<()>> + Send;
    /// Discard any pending write-complete interrupts so the next
    /// await_write_complete only sees interrupts that fire from now on.
    /// Called during error recovery to re-synchronize with the firmware.
    fn drain_write_complete(&self) -> impl std::future::Future<Output = ()> + Send;

    /// Read and discard whatever the adapter still owes us, so an abandoned
    /// transfer cannot desynchronise the next one.
    ///
    /// Defaulted to a no-op: this is a property of a real USB pipe, and a mock
    /// transport with pre-queued responses would have them eaten.
    fn drain_bulk_in(&self) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    /// Receiver for service-request notifications, when the transport has a
    /// path that reports them. `None` means this transport cannot observe SRQ,
    /// which callers must treat as "unknown", never as "no SRQ".
    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        None
    }
}

pub struct GpibController<T: Transport> {
    pub transport: T,
    pub timeout_ms: u32,
    pub eos_char: u8,
    pub eos_enabled: bool,
    pub hw_control_bits: u8,
    pub listen_only: bool,
}

impl<T: Transport> GpibController<T> {
    pub fn new(transport: T, timeout_ms: u32) -> Self {
        Self {
            transport,
            timeout_ms,
            eos_char: b'\n',
            eos_enabled: false,
            hw_control_bits: 0,
            listen_only: false,
        }
    }

    pub async fn write_registers(&mut self, regs: &[RegisterPairlet]) -> Result<()> {
        let pkt = encode_wr_regs(regs);
        self.transport.write_bulk(&pkt).await?;
        // Bound register-response wait. If the device is wedged the bulk-in
        // never completes; we'd rather surface an error than block forever.
        let resp = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms as u64),
            self.transport.read_bulk(0x20),
        )
        .await
        .map_err(|_| anyhow::anyhow!("WR_REGS bulk-in timed out"))??;
        decode_wr_regs_response(&resp)?;
        Ok(())
    }

    pub async fn read_registers(&mut self, regs: &mut [RegisterPairlet]) -> Result<()> {
        let addrs: Vec<u8> = regs.iter().map(|r| r.address).collect();
        let pkt = encode_rd_regs(&addrs);
        self.transport.write_bulk(&pkt).await?;
        let resp = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms as u64),
            self.transport.read_bulk(0x20),
        )
        .await
        .map_err(|_| anyhow::anyhow!("RD_REGS bulk-in timed out"))??;
        decode_rd_regs_response(&resp, regs)?;
        Ok(())
    }

    /// Initialize the GPIB controller. `my_pad` is our primary address (always 0).
    /// Matches the kernel driver's `agilent_82357a_init()` with t1_nano_sec=800,
    /// then requests system controller and pulses IFC + asserts REN.
    pub async fn init(&mut self, my_pad: u8) -> Result<()> {
        // Clear anything a dead predecessor left queued before trusting a
        // single response byte. Without this a fresh daemon fails its first
        // init with `unexpected response byte 0xfa, expected 0xfb` and
        // succeeds on the second — the retry "works" only by consuming the
        // leftovers, which reads as flakiness rather than as a bug.
        self.transport.drain_bulk_in().await;
        // Batch 1: light FAIL LED and pulse reset
        let batch1 = [
            RegisterPairlet {
                address: REG_LED_CONTROL,
                value: FAIL_LED_ON,
            },
            RegisterPairlet {
                address: REG_RESET_TO_POWERUP,
                value: RESET_SPACEBALL,
            },
        ];
        self.write_registers(&batch1).await?;

        // 2 ms settle
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        // Fast-talker T1 bits for 800 ns, clamped to valid register range.
        let t1_bits: u8 = (800u32 / 21).clamp(0x11, 0x72) as u8;
        let batch2 = [
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_NBAF,
            },
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_HLDE,
            },
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_TON,
            },
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_LON,
            },
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_RSV2,
            },
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_INVAL,
            },
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_RPP,
            },
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_STDL,
            },
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_VSTDL,
            },
            RegisterPairlet {
                address: REG_FAST_TALKER_T1,
                value: t1_bits,
            },
            RegisterPairlet {
                address: TMS_ADR,
                value: my_pad & ADDRESS_MASK,
            },
            RegisterPairlet {
                address: TMS_PPR,
                value: 0,
            },
            RegisterPairlet {
                address: TMS_SPMR,
                value: 0,
            },
            RegisterPairlet {
                address: REG_PROTOCOL_CONTROL,
                value: WRITE_COMPLETE_INTERRUPT_EN,
            },
            RegisterPairlet {
                address: TMS_IMR0,
                value: HR_BOIE | HR_BIIE,
            },
            RegisterPairlet {
                address: TMS_IMR1,
                value: HR_SRQIE,
            },
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_CHIP_RESET,
            },
            RegisterPairlet {
                address: REG_LED_CONTROL,
                value: FIRMWARE_LED_CONTROL,
            },
        ];
        self.write_registers(&batch2).await?;

        // Read back HW_CONTROL and stash it
        let mut hw = [RegisterPairlet {
            address: REG_HW_CONTROL,
            value: 0,
        }];
        self.read_registers(&mut hw).await?;
        self.hw_control_bits = (hw[0].value & !0x07) | NOT_TI_RESET | NOT_PARALLEL_POLL;

        self.request_system_control().await?;
        self.ifc().await?;
        self.ren(true).await?;

        Ok(())
    }

    async fn request_system_control(&mut self) -> Result<()> {
        self.hw_control_bits |= SYSTEM_CONTROLLER;
        let regs = [
            RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_RQC,
            },
            RegisterPairlet {
                address: REG_HW_CONTROL,
                value: self.hw_control_bits,
            },
        ];
        self.write_registers(&regs).await
    }

    /// Send Interface Clear pulse (~200 µs: assert then deassert).
    pub async fn ifc(&mut self) -> Result<()> {
        let assert_ = [RegisterPairlet {
            address: TMS_AUXCR,
            value: AUX_SIC | AUX_CS,
        }];
        self.write_registers(&assert_).await?;
        tokio::time::sleep(std::time::Duration::from_micros(200)).await;
        let deassert = [RegisterPairlet {
            address: TMS_AUXCR,
            value: AUX_SIC,
        }];
        self.write_registers(&deassert).await?;
        Ok(())
    }

    /// Assert or deassert Remote Enable.
    pub async fn ren(&mut self, enable: bool) -> Result<()> {
        let value = if enable { AUX_SRE | AUX_CS } else { AUX_SRE };
        let reg = [RegisterPairlet {
            address: TMS_AUXCR,
            value,
        }];
        self.write_registers(&reg).await
    }

    /// Send Selected Device Clear to `pad` (SDC, preceded by addressing).
    ///
    /// SDC is an addressed command, and addressed commands act on devices
    /// addressed to *listen*. Addressing the target as a talker leaves SDC with
    /// no listener to act on, so the clear is silently a no-op.
    pub async fn device_clear(&mut self, pad: u8) -> Result<()> {
        let cmd = [GPIB_UNL, listen_address(pad), GPIB_SDC];
        self.send_command_bytes(&cmd).await
    }

    /// Address `pad` as listener, which is the transition into remote state
    /// while REN is asserted.
    pub async fn go_to_remote(&mut self, pad: u8) -> Result<()> {
        self.ren(true).await?;
        let cmd = [GPIB_UNL, listen_address(pad)];
        self.send_command_bytes(&cmd).await
    }

    /// Send Go To Local to `pad` (GTL, addressed as listener), returning that
    /// one device to front-panel control without disturbing the rest of the
    /// bus the way dropping REN would.
    pub async fn go_to_local(&mut self, pad: u8) -> Result<()> {
        let cmd = [GPIB_UNL, listen_address(pad), GPIB_GTL];
        self.send_command_bytes(&cmd).await
    }

    /// Send Local Lockout (LLO). Universal, so it takes no address and applies
    /// to every device on the bus.
    pub async fn local_lockout(&mut self) -> Result<()> {
        self.send_command_bytes(&[GPIB_LLO]).await
    }

    /// Send Group Execute Trigger to `pad` (GET, addressed as listener).
    pub async fn trigger(&mut self, pad: u8) -> Result<()> {
        let cmd = [GPIB_UNL, listen_address(pad), GPIB_GET];
        self.send_command_bytes(&cmd).await
    }

    /// Write `data` to instrument at `pad`. Handles GPIB addressing internally.
    ///
    /// The sequence must begin with UNL. Listener addressing is cumulative: a
    /// device told to listen stays listening until something unaddresses it, so
    /// without UNL every device addressed by an earlier write would still be
    /// listening and would receive this one too. Untalk does not help — it
    /// clears talkers, not listeners. Making ourselves the talker with MTA is
    /// what unaddresses any previous talker, since there can only be one.
    pub async fn write(&mut self, pad: u8, data: &[u8], send_eoi: bool) -> Result<()> {
        if self.listen_only {
            anyhow::bail!("cannot write while in listen-only mode (++lon 0 to leave)");
        }
        let addr_cmd = [GPIB_UNL, talk_address(0), listen_address(pad)];
        self.send_command_bytes(&addr_cmd).await?;
        self.send_data_bytes(data, send_eoi).await
    }

    /// Read up to `max_len` bytes from instrument at `pad`. Returns (data, end_of_message).
    /// On timeout, aborts the pending transfer AND pulses IFC to return all
    /// bus participants to idle — otherwise a device left addressed as talker
    /// will hang the next transaction.
    pub async fn read(&mut self, pad: u8, max_len: usize) -> Result<(Vec<u8>, bool)> {
        // In listen-only, address ourselves as sole listener and designate no
        // talker: the talk-only source is already talking and has no address to
        // name. Measured on the NI backend — sending no command bytes at all
        // captures nothing, and naming `MTA(0)` designates a talker that does
        // not exist. See `docs/CAPTURE.md` §14.15.
        // In listen-only we were addressed when the mode was entered and are in
        // standby; re-addressing per read would assert ATN on every re-arm and
        // abort a transfer in flight.
        if !self.listen_only {
            let addr_cmd = [GPIB_UNL, listen_address(0), talk_address(pad)];
            self.send_command_bytes(&addr_cmd).await?;
        }
        let gts = [RegisterPairlet {
            address: TMS_AUXCR,
            value: AUX_GTS,
        }];
        self.write_registers(&gts).await?;
        let pkt = encode_gpib_read(
            max_len as u32,
            self.eos_enabled,
            self.eos_char,
            // End on EOI even while capturing. Removing it was a mistake:
            // with no termination flag the firmware can only end the read on
            // count, and `max_len` is 64 KiB, so a print smaller than that
            // never completes — it times out, and the timeout path aborts and
            // drains, discarding every byte it had collected.
            //
            // The re-arm gap this was meant to avoid was a theory about the NI
            // adapter that turned out to be wrong; the real fault there was the
            // addressing (§14.12).
            true,
        );
        self.transport.write_bulk(&pkt).await?;

        let read_fut = self.transport.read_bulk(max_len + 1);
        let timeout = std::time::Duration::from_millis(self.timeout_ms as u64);
        match tokio::time::timeout(timeout, read_fut).await {
            Ok(Ok(raw)) => Ok(decode_gpib_read_response(&raw)),
            Ok(Err(e)) => {
                self.recover_from_stall().await;
                Err(e)
            }
            Err(_) => {
                // Do NOT pulse IFC while capturing. `recover_from_stall` ends
                // with an interface clear, which is right for a wedged
                // addressed transfer and catastrophic for a capture: a read
                // timeout is the *normal* idle state of a capture loop, and
                // IFC would reset the bus, knock a talk-only instrument out of
                // its mode, and discard the transfer it was meant to salvage.
                // This is `docs/CAPTURE.md` §4.1, observed rather than
                // predicted: on the 82357 it silently destroyed every capture.
                if self.listen_only {
                    let _ = self.abort(true).await;
                    let _ = self.abort(false).await;
                    self.transport.drain_write_complete().await;
                    // Resynchronise the bulk pipe. `tokio::time::timeout`
                    // above *drops* the in-flight read rather than waiting for
                    // it, so the adapter's response to the abandoned transfer
                    // arrives afterwards with nobody expecting it — and the
                    // next read consumes it as data. The symptom is a lone
                    // `0xfb` in the capture stream, which is this adapter's
                    // own WR_REGS response code, not instrument bytes.
                    //
                    // A capture read times out by design on every quiet
                    // interval, so without this the pipe desynchronises within
                    // seconds of entering the mode.
                    self.transport.drain_bulk_in().await;
                } else {
                    self.recover_from_stall().await;
                }
                anyhow::bail!("gpib read timed out after {} ms", self.timeout_ms)
            }
        }
    }

    /// Serial-poll the instrument at `pad` and return its status byte.
    ///
    /// Addresses the instrument as talker and ourselves as listener with Serial
    /// Poll Enable asserted, drops to standby so it can drive the byte, reads
    /// exactly one byte, then restores the bus with Serial Poll Disable and
    /// untalk. The status byte is binary, so this read must not terminate on the
    /// EOS character — a status of 0x0a would otherwise look like a terminator.
    ///
    /// SPD/UNT is sent even when the read fails, so a non-responding instrument
    /// cannot leave the whole bus stuck in serial-poll mode.
    pub async fn serial_poll(&mut self, pad: u8) -> Result<u8> {
        let enable = [
            GPIB_UNL,
            GPIB_SPE,
            talk_address(pad),
            listen_address(0), // we are the controller at pad 0
        ];
        self.send_command_bytes(&enable).await?;

        let gts = [RegisterPairlet {
            address: TMS_AUXCR,
            value: AUX_GTS,
        }];
        self.write_registers(&gts).await?;

        let pkt = encode_gpib_read(1, false, 0, true);
        self.transport.write_bulk(&pkt).await?;

        // One data byte plus the trailing flags byte.
        let read_fut = self.transport.read_bulk(2);
        let timeout = std::time::Duration::from_millis(self.timeout_ms as u64);
        let outcome = match tokio::time::timeout(timeout, read_fut).await {
            Ok(Ok(raw)) => Ok(decode_gpib_read_response(&raw).0),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow::anyhow!(
                "serial poll of pad {pad} timed out after {} ms",
                self.timeout_ms
            )),
        };

        match outcome {
            Ok(data) => {
                self.send_command_bytes(&[GPIB_SPD, GPIB_UNT]).await?;
                data.first().copied().ok_or_else(|| {
                    anyhow::anyhow!("serial poll of pad {pad} returned no status byte")
                })
            }
            Err(e) => {
                self.recover_from_stall().await;
                Err(e)
            }
        }
    }

    /// Whether some device on the bus is currently asserting SRQ.
    ///
    /// A live read of the physical line, not an event. The interrupt endpoint
    /// reports SRQ as an *edge*, which is not enough on a shared bus: SRQ is
    /// wired-OR, so a device that asserts while another is already holding the
    /// line low produces no edge and is never announced. Callers use this to
    /// tell "still asserted, keep looking" from "released, nothing pending".
    pub async fn srq_asserted(&mut self) -> Result<bool> {
        Ok(self.bus_lines().await?.srq)
    }

    /// Enter or leave unaddressed-listen. **Does not work on this adapter yet.**
    ///
    /// The chip reaches the right state — `0x21 NDAC REN`, a genuinely ready
    /// listener, better than the NI manages — but no bytes are collected. See
    /// `docs/CAPTURE.md` §14.15, which records four theories tried and refuted
    /// so they are not tried again. Kept rather than removed so that "tried and
    /// does not work" stays distinguishable from "nobody tried", per the
    /// roadmap's rule about untested adapters.
    pub async fn set_listen_only(&mut self, enable: bool) -> Result<()> {
        // On the TMS9914 the aux-command set/clear bit is AUX_CS, so 0x89 sets
        // listen-only and 0x09 clears it.
        let mut regs = vec![RegisterPairlet {
            address: TMS_AUXCR,
            value: if enable { AUX_LON | AUX_CS } else { AUX_LON },
        }];
        if enable {
            // Release any DAC holdoff already in effect, or the talker stays
            // blocked on the byte we are sitting on. Note init already clears
            // holdoff-on-EOI (it writes AUX_HLDE without AUX_CS), so unlike the
            // TNT4882 there is no holdoff *mode* left to turn off here.
            regs.push(RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_VAL,
            });
        }
        self.write_registers(&regs).await?;
        if enable {
            // Also announce ourselves as a listener on the bus, in case the
            // adapter arms its read engine off the addressing rather than off
            // the listen-only bit. (It does not appear to — see
            // `docs/CAPTURE.md` §14.15 — but the command is harmless and the
            // NI needs its equivalent.)
            //
            // Failure is expected and ignored: with only a talk-only
            // instrument on the bus these command bytes have no acceptor. What
            // must not happen is the IFC that `send_command_bytes` pulses on
            // failure, which would reset the bus and undo the mode.
            if let Err(e) = self.try_command_bytes(&[GPIB_UNL, listen_address(0)]).await {
                tracing::debug!("listen-only self-addressing not accepted: {e:#}");
                self.transport.drain_bulk_in().await;
            }
            // Go to standby *after* the addressing. Command bytes are sent with
            // ATN asserted, so doing this first leaves ATN up afterwards — and
            // with ATN asserted no data can move at all.
            let gts = [RegisterPairlet {
                address: TMS_AUXCR,
                value: AUX_GTS,
            }];
            self.write_registers(&gts).await?;
        }
        // Beyond that, no addressing per read, unlike the NI backend.
        //
        // `AUX_LON | AUX_CS` is a *true* hardware listen-only on the TMS9914
        // (`tms9914.h:262`, with the `AUX_CS` set/clear convention), where the
        // TNT4882's `HR_LON` is not enough on its own and has to reach LACS
        // through the addressing path. Measured: this sequence alone put the
        // 82357 at `0x21 NDAC REN`, a ready listener, which is better than the
        // NI ever manages.
        //
        // Adding `UNL, MLA(0)` here made it worse, and instructively. Those
        // command bytes need an acceptor, and with only a talk-only instrument
        // on the bus there is none, so `send_command_bytes` failed — and its
        // failure path calls `recover_from_stall`, which pulses IFC and leaves
        // the bulk pipe dirty. The capture then delivered a single `0xfb`,
        // which is this adapter's own WR_REGS response code rather than
        // instrument data.
        self.listen_only = enable;
        tracing::info!(listen_only = enable, "unaddressed listen (82357)");
        Ok(())
    }

    /// Read the bus status register and decode all eight control lines.
    ///
    /// Reading `BSR` has no side effects, unlike ISR0/ISR1 at offsets 0 and 1,
    /// which clear pending interrupt status on read and would steal
    /// notifications from the interrupt endpoint.
    pub async fn bus_lines(&mut self) -> Result<crate::backend::BusLines> {
        let mut regs = [RegisterPairlet {
            address: TMS_BSR,
            value: 0,
        }];
        self.read_registers(&mut regs).await?;
        let lines = crate::backend::BusLines::from_bsr(regs[0].value);
        tracing::debug!(bus = %lines, "bus status");
        Ok(lines)
    }

    /// Best-effort recovery after a stalled transfer: flush the in-flight USB
    /// transfer, drain any partial bulk-in data and any late write-complete
    /// interrupts, finalize the abort, then pulse IFC to reset the GPIB bus.
    /// Errors are swallowed — the caller is already propagating a failure.
    async fn recover_from_stall(&mut self) {
        tracing::debug!("recover_from_stall: abort(flush), drain, abort, ifc");
        let _ = self.abort(true).await;

        let drain_fut = self.transport.read_bulk(0x20);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), drain_fut).await;

        let _ = self.abort(false).await;

        // Discard any late write-complete interrupts that the firmware fired
        // after the abort — otherwise they'd prematurely satisfy the next
        // await_write_complete call.
        self.transport.drain_write_complete().await;

        let _ = self.ifc().await;

        // IFC itself produces register writes which we've already awaited;
        // drain again just to be safe.
        self.transport.drain_write_complete().await;
    }

    /// Send command bytes without the stall recovery.
    ///
    /// `send_command_bytes` pulses IFC when a command is not accepted, which is
    /// right for a wedged addressed transfer and wrong when the command is
    /// speculative. Addressing ourselves as listener on a bus whose only other
    /// device is talk-only has no acceptor and legitimately fails; resetting
    /// the bus over that would undo the mode we are trying to enter.
    async fn try_command_bytes(&mut self, cmd: &[u8]) -> Result<()> {
        let pkt = encode_gpib_command(cmd);
        self.transport.write_bulk(&pkt).await?;
        self.wait_write_or_timeout().await?;
        self.get_xfer_status().await?;
        Ok(())
    }

    async fn send_command_bytes(&mut self, cmd: &[u8]) -> Result<()> {
        let pkt = encode_gpib_command(cmd);
        self.transport.write_bulk(&pkt).await?;
        // Race the write-complete interrupt against the GPIB timeout. The
        // control-IN for XFER_STATUS that follows acts as the real sync
        // point — the firmware only responds to it after the bulk write is
        // actually complete — but the interrupt gives us an early signal so
        // back-to-back writes don't stall unnecessarily.
        if let Err(e) = self.wait_write_or_timeout().await {
            self.recover_from_stall().await;
            return Err(e);
        }
        if let Err(e) = self.get_xfer_status().await {
            self.recover_from_stall().await;
            return Err(e);
        }
        Ok(())
    }

    async fn send_data_bytes(&mut self, data: &[u8], send_eoi: bool) -> Result<()> {
        let pkt = encode_gpib_write(data, send_eoi);
        self.transport.write_bulk(&pkt).await?;
        if let Err(e) = self.wait_write_or_timeout().await {
            self.recover_from_stall().await;
            return Err(e);
        }
        if let Err(e) = self.get_xfer_status().await {
            self.recover_from_stall().await;
            return Err(e);
        }
        Ok(())
    }

    /// Wait for a write-complete interrupt bounded by `timeout_ms`. Returns
    /// Ok even if a *stale* permit from a previous operation satisfies it —
    /// the subsequent XFER_STATUS control transfer acts as the authoritative
    /// sync point, so a false-early return is harmless and matches the kernel
    /// driver's permit-style behavior.
    async fn wait_write_or_timeout(&mut self) -> Result<()> {
        self.transport.await_write_complete().await
    }

    /// Issue XFER_ABORT control transfer. `flush` cancels an in-progress
    /// bulk transfer; without flush it just finalizes an aborted state.
    pub async fn abort(&mut self, flush: bool) -> Result<()> {
        let idx = if flush { XA_FLUSH } else { 0 };
        let resp = self
            .transport
            .control_in(CONTROL_REQUEST, XFER_ABORT, idx, 2)
            .await?;
        if resp.len() < 2 {
            anyhow::bail!("XFER_ABORT response too short: {} bytes", resp.len());
        }
        let expected = !(XFER_ABORT as u8);
        if resp[0] != expected {
            anyhow::bail!(
                "XFER_ABORT bad response byte: got {:#x}, expected {:#x}",
                resp[0],
                expected
            );
        }
        match resp[1] {
            UGP_SUCCESS => Ok(()),
            // "already flushing" is fine when we asked for flush
            UGP_ERR_FLUSHING if flush => Ok(()),
            code => anyhow::bail!("XFER_ABORT returned error {:#x}", code),
        }
    }

    /// Issue XFER_STATUS control transfer and return bytes-written count.
    async fn get_xfer_status(&mut self) -> Result<u32> {
        let resp = self
            .transport
            .control_in(CONTROL_REQUEST, XFER_STATUS, 0, STATUS_DATA_LEN)
            .await?;
        if resp.len() < 6 {
            anyhow::bail!("XFER_STATUS response too short: {} bytes", resp.len());
        }
        Ok(u32::from_le_bytes([resp[2], resp[3], resp[4], resp[5]]))
    }
}

// Expose the 82357B controller through the adapter-agnostic backend trait.
// The generic GPIB operations already exist as inherent methods above (which
// take precedence in resolution, so these forward rather than recurse); the
// 82357B-specific machinery — register I/O, firmware, abort/XFER control — stays
// off the trait as this backend's private detail.
#[async_trait::async_trait]
impl<T: Transport + Send + Sync + 'static> crate::backend::GpibBackend for GpibController<T> {
    async fn init(&mut self, my_pad: u8) -> Result<()> {
        self.init(my_pad).await
    }
    async fn write(&mut self, pad: u8, data: &[u8], send_eoi: bool) -> Result<()> {
        self.write(pad, data, send_eoi).await
    }
    async fn read(&mut self, pad: u8, max_len: usize) -> Result<(Vec<u8>, bool)> {
        self.read(pad, max_len).await
    }
    async fn device_clear(&mut self, pad: u8) -> Result<()> {
        self.device_clear(pad).await
    }
    async fn trigger(&mut self, pad: u8) -> Result<()> {
        self.trigger(pad).await
    }
    async fn go_to_remote(&mut self, pad: u8) -> Result<()> {
        self.go_to_remote(pad).await
    }
    async fn go_to_local(&mut self, pad: u8) -> Result<()> {
        self.go_to_local(pad).await
    }
    async fn local_lockout(&mut self) -> Result<()> {
        self.local_lockout().await
    }
    async fn ifc(&mut self) -> Result<()> {
        self.ifc().await
    }
    async fn ren(&mut self, enable: bool) -> Result<()> {
        self.ren(enable).await
    }
    async fn serial_poll(&mut self, pad: u8) -> Result<u8> {
        self.serial_poll(pad).await
    }
    async fn srq_asserted(&mut self) -> Result<bool> {
        self.srq_asserted().await
    }
    async fn bus_lines(&mut self) -> Result<crate::backend::BusLines> {
        self.bus_lines().await
    }
    async fn set_listen_only(&mut self, enable: bool) -> Result<()> {
        self.set_listen_only(enable).await
    }
    fn listen_only(&self) -> bool {
        self.listen_only
    }
    fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        self.transport.subscribe_srq()
    }
    fn set_eos(&mut self, eos_char: u8, enabled: bool) {
        self.eos_char = eos_char;
        self.eos_enabled = enabled;
    }
    fn set_timeout(&mut self, timeout_ms: u32) {
        self.timeout_ms = timeout_ms;
    }
    fn name(&self) -> &'static str {
        // Family id: the shared controller is not told which model opened it.
        // The specific backend id is available via the registry (BackendKind).
        "agilent-82357"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    pub(crate) struct MockTransport {
        pub written: Mutex<Vec<Vec<u8>>>,
        pub responses: Mutex<Vec<Vec<u8>>>,
        pub control_responses: Mutex<Vec<Vec<u8>>>,
    }

    impl MockTransport {
        pub fn new() -> Self {
            Self {
                written: Mutex::new(vec![]),
                responses: Mutex::new(vec![]),
                control_responses: Mutex::new(vec![]),
            }
        }
        pub fn push_response(&self, r: Vec<u8>) {
            self.responses.lock().unwrap().push(r);
        }
        pub fn push_control(&self, r: Vec<u8>) {
            self.control_responses.lock().unwrap().push(r);
        }
        pub fn last_written(&self) -> Vec<u8> {
            self.written.lock().unwrap().last().unwrap().clone()
        }
    }

    impl Transport for MockTransport {
        async fn write_bulk(&self, data: &[u8]) -> Result<()> {
            self.written.lock().unwrap().push(data.to_vec());
            Ok(())
        }
        async fn read_bulk(&self, _max: usize) -> Result<Vec<u8>> {
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                anyhow::bail!("mock: read_bulk called with no response queued");
            }
            Ok(q.remove(0))
        }
        async fn control_in(&self, _req: u8, _val: u16, _idx: u16, _max: usize) -> Result<Vec<u8>> {
            let mut q = self.control_responses.lock().unwrap();
            if q.is_empty() {
                anyhow::bail!("mock: control_in called with no response queued");
            }
            Ok(q.remove(0))
        }
        async fn await_write_complete(&self) -> Result<()> {
            Ok(())
        }
        async fn drain_write_complete(&self) {}
    }

    pub(crate) fn wr_regs_ok() -> Vec<u8> {
        vec![!(BulkCmd::WrRegs as u8), 0, 0, 0, 0, 0, 0, 0]
    }

    fn xfer_status(n: u32) -> Vec<u8> {
        let mut v = vec![0u8; 8];
        v[2..6].copy_from_slice(&n.to_le_bytes());
        v
    }

    #[tokio::test]
    async fn write_registers_sends_correct_packet() {
        let t = MockTransport::new();
        t.push_response(wr_regs_ok());
        let mut ctrl = GpibController::new(t, 3000);
        let regs = &[RegisterPairlet {
            address: 0x0a,
            value: 0x01,
        }];
        ctrl.write_registers(regs).await.unwrap();
        let sent = ctrl.transport.last_written();
        assert_eq!(sent[0], BulkCmd::WrRegs as u8);
        assert_eq!(sent[1], 1);
        assert_eq!(sent[2], 0x0a);
        assert_eq!(sent[3], 0x01);
    }

    #[tokio::test]
    async fn read_registers_sends_and_parses() {
        let t = MockTransport::new();
        t.push_response(vec![!(BulkCmd::RdRegs as u8), 0x00, 0x42]);
        let mut ctrl = GpibController::new(t, 3000);
        let mut regs = vec![RegisterPairlet {
            address: 0x0a,
            value: 0,
        }];
        ctrl.read_registers(&mut regs).await.unwrap();
        assert_eq!(regs[0].value, 0x42);
    }

    #[tokio::test]
    async fn init_sends_correct_sequence() {
        let t = MockTransport::new();
        // init() issues, in order:
        //   1. WR_REGS batch1 (2 regs)
        //   2. WR_REGS batch2 (18 regs)
        //   3. RD_REGS HW_CONTROL
        //   4. WR_REGS request_system_control (2 regs)
        //   5. WR_REGS ifc assert
        //   6. WR_REGS ifc deassert
        //   7. WR_REGS ren(true)
        for _ in 0..2 {
            t.push_response(wr_regs_ok());
        }
        t.push_response(vec![!(BulkCmd::RdRegs as u8), 0x00, 0b10101010]);
        for _ in 0..4 {
            t.push_response(wr_regs_ok());
        }
        let mut ctrl = GpibController::new(t, 3000);
        ctrl.init(0).await.unwrap();
        // hw_control stashed with top bits from device, low 3 cleared, | NOT_TI_RESET | NOT_PARALLEL_POLL | SYSTEM_CONTROLLER
        let expected = (0b10101010 & !0x07) | NOT_TI_RESET | NOT_PARALLEL_POLL | SYSTEM_CONTROLLER;
        assert_eq!(ctrl.hw_control_bits, expected);
    }

    #[tokio::test]
    async fn gpib_write_sends_addressing_then_data() {
        let t = MockTransport::new();
        // 2 control_in responses (ATN command, then data bytes)
        t.push_control(xfer_status(3));
        t.push_control(xfer_status(6));
        let mut ctrl = GpibController::new(t, 3000);
        ctrl.write(15, b"*IDN?", true).await.unwrap();
        let writes = ctrl.transport.written.lock().unwrap().clone();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0][0], BulkCmd::Write as u8);
        let cmd_flags = writes[0][3];
        assert!(cmd_flags & WriteFlag::Atn as u8 != 0);
        assert_eq!(
            cmd_payload(&writes[0]),
            [GPIB_UNL, talk_address(0), listen_address(15)]
        );
        assert_eq!(writes[1][0], BulkCmd::Write as u8);
        let data_flags = writes[1][3];
        assert!(data_flags & WriteFlag::NoAddress as u8 != 0);
        assert!(data_flags & WriteFlag::SendEoi as u8 != 0);
    }

    /// Listener addressing is cumulative, so every write must unaddress the
    /// previous one's listener. Without the leading UNL a write to pad 3 is
    /// also delivered to pad 23 — observed on a real two-instrument bus, where
    /// a command sent to a 34401A also reached a 53132A.
    #[tokio::test]
    async fn consecutive_writes_to_different_pads_each_unlisten_first() {
        let t = MockTransport::new();
        for _ in 0..4 {
            t.push_control(xfer_status(3));
        }
        let mut ctrl = GpibController::new(t, 3000);

        ctrl.write(23, b"A", true).await.unwrap();
        ctrl.write(3, b"B", true).await.unwrap();

        let writes = ctrl.transport.written.lock().unwrap().clone();
        // writes[0]/[2] are the addressing commands, [1]/[3] the data.
        assert_eq!(
            cmd_payload(&writes[0]),
            [GPIB_UNL, talk_address(0), listen_address(23)]
        );
        assert_eq!(
            cmd_payload(&writes[2]),
            [GPIB_UNL, talk_address(0), listen_address(3)],
            "second write must unlisten pad 23 before addressing pad 3"
        );
    }

    #[tokio::test]
    async fn gpib_read_sends_addressing_then_read() {
        let t = MockTransport::new();
        // send_command_bytes -> 1 control_in for XFER_STATUS
        t.push_control(xfer_status(3));
        // write_registers for GTS -> 1 wr_regs_ok read_bulk response
        t.push_response(wr_regs_ok());
        // final read_bulk: data + trailing flag byte
        let mut read_resp = b"KEYSIGHT,34461A\n".to_vec();
        read_resp.push(ATRF_EOI);
        t.push_response(read_resp);
        let mut ctrl = GpibController::new(t, 3000);
        let (data, eom) = ctrl.read(15, 4096).await.unwrap();
        assert_eq!(data, b"KEYSIGHT,34461A\n");
        assert!(eom);
        let writes = ctrl.transport.written.lock().unwrap().clone();
        // First write is ATN command [UNL, MLA(0), TAD(15)]
        let cmd_len =
            u32::from_le_bytes([writes[0][4], writes[0][5], writes[0][6], writes[0][7]]) as usize;
        let cmd_bytes = &writes[0][8..8 + cmd_len];
        assert!(cmd_bytes.contains(&0x3f), "cmd_bytes={cmd_bytes:?}"); // UNL
        assert!(cmd_bytes.contains(&0x20)); // MLA(0)
        assert!(cmd_bytes.contains(&(0x40 + 15))); // TAD(15)
    }

    /// Extract the GPIB bytes carried by a DATA_PIPE_CMD_WRITE packet.
    fn cmd_payload(pkt: &[u8]) -> &[u8] {
        let len = u32::from_le_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]) as usize;
        &pkt[8..8 + len]
    }

    #[tokio::test]
    async fn serial_poll_addresses_polls_then_restores_bus() {
        let t = MockTransport::new();
        t.push_control(xfer_status(4)); // SPE addressing
        t.push_control(xfer_status(2)); // SPD/UNT
        t.push_response(wr_regs_ok()); // GTS register write
        t.push_response(vec![0x51, ATRF_EOI]); // status byte + trailing flags
        let mut ctrl = GpibController::new(t, 3000);

        let stb = ctrl.serial_poll(23).await.unwrap();
        assert_eq!(stb, 0x51);

        let writes = ctrl.transport.written.lock().unwrap().clone();
        assert_eq!(
            cmd_payload(&writes[0]),
            [GPIB_UNL, GPIB_SPE, talk_address(23), listen_address(0)]
        );
        // The bus must be taken back out of serial-poll mode afterwards.
        assert_eq!(cmd_payload(writes.last().unwrap()), [GPIB_SPD, GPIB_UNT]);
    }

    #[tokio::test]
    async fn serial_poll_read_ignores_the_eos_terminator() {
        let t = MockTransport::new();
        t.push_control(xfer_status(4));
        t.push_control(xfer_status(2));
        t.push_response(wr_regs_ok());
        // A status byte that happens to equal the configured EOS character.
        t.push_response(vec![b'\n', ATRF_EOI]);
        let mut ctrl = GpibController::new(t, 3000);
        ctrl.eos_char = b'\n';
        ctrl.eos_enabled = true;

        assert_eq!(ctrl.serial_poll(5).await.unwrap(), b'\n');

        // writes[2] is the read request; terminating on EOS would truncate a
        // binary status byte, so that flag must stay clear regardless of eos.
        let writes = ctrl.transport.written.lock().unwrap().clone();
        assert_eq!(writes[2][3] & ReadFlag::EndOnEosChar as u8, 0);
    }

    #[tokio::test]
    async fn serial_poll_reports_a_missing_status_byte_as_an_error() {
        let t = MockTransport::new();
        t.push_control(xfer_status(4));
        t.push_control(xfer_status(2));
        t.push_response(wr_regs_ok());
        t.push_response(vec![ATRF_EOI]); // trailing flags only, no data byte
        let mut ctrl = GpibController::new(t, 3000);

        let err = ctrl.serial_poll(9).await.unwrap_err();
        assert!(
            err.to_string().contains("no status byte"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn send_ifc() {
        let t = MockTransport::new();
        t.push_response(wr_regs_ok()); // assert
        t.push_response(wr_regs_ok()); // deassert
        let mut ctrl = GpibController::new(t, 3000);
        ctrl.ifc().await.unwrap();
        let written = ctrl.transport.written.lock().unwrap().clone();
        assert_eq!(written[0][0], BulkCmd::WrRegs as u8);
        assert_eq!(written[1][0], BulkCmd::WrRegs as u8);
    }

    #[tokio::test]
    async fn device_clear_sends_sdc_addressing() {
        let t = MockTransport::new();
        t.push_control(xfer_status(3));
        let mut ctrl = GpibController::new(t, 3000);
        ctrl.device_clear(7).await.unwrap();
        let writes = ctrl.transport.written.lock().unwrap().clone();
        // SDC acts on listeners. Addressing pad 7 to talk instead would leave
        // the command with nothing to act on and silently clear nothing.
        assert_eq!(
            cmd_payload(&writes[0]),
            [GPIB_UNL, listen_address(7), GPIB_SDC]
        );
    }

    #[tokio::test]
    async fn go_to_local_is_addressed_to_the_listener() {
        let t = MockTransport::new();
        t.push_control(xfer_status(3));
        let mut ctrl = GpibController::new(t, 3000);
        ctrl.go_to_local(7).await.unwrap();
        let writes = ctrl.transport.written.lock().unwrap().clone();
        // GTL is an addressed command, so it acts only on this instrument —
        // that is the whole point of it over dropping REN, which returns every
        // device on the bus to local.
        assert_eq!(
            cmd_payload(&writes[0]),
            [GPIB_UNL, listen_address(7), GPIB_GTL]
        );
    }

    #[tokio::test]
    async fn local_lockout_is_universal_and_unaddressed() {
        let t = MockTransport::new();
        t.push_control(xfer_status(1));
        let mut ctrl = GpibController::new(t, 3000);
        ctrl.local_lockout().await.unwrap();
        let writes = ctrl.transport.written.lock().unwrap().clone();
        // LLO takes no address: IEEE-488 defines no per-device lockout, and
        // addressing it would be inventing one.
        assert_eq!(cmd_payload(&writes[0]), [GPIB_LLO]);
    }

    #[tokio::test]
    async fn go_to_remote_asserts_ren_then_addresses() {
        let t = MockTransport::new();
        t.push_response(wr_regs_ok()); // ren(true)
        t.push_control(xfer_status(2));
        let mut ctrl = GpibController::new(t, 3000);
        ctrl.go_to_remote(7).await.unwrap();
        let writes = ctrl.transport.written.lock().unwrap().clone();
        // REN only permits remote; the listen address is what performs the
        // transition, so both are needed and in this order.
        assert_eq!(writes[0][0], BulkCmd::WrRegs as u8);
        assert_eq!(cmd_payload(&writes[1]), [GPIB_UNL, listen_address(7)]);
    }
}
