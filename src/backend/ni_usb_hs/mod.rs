// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// National Instruments GPIB-USB-HS backend, translated from the Linux kernel
// `drivers/gpib/ni_usb/ni_usb_gpib.c`. Unlike the 82357B this adapter needs no
// firmware upload — it boots ready and performs a control-endpoint readiness
// handshake instead.
//
// Brought up and verified on a physical GPIB-USB-HS (PID 0x709b) against an
// SR620: identify, query, serial poll, device clear, and daemon restart.
//
// The HS+ (0x7618), KUSB-488A and MC-USB-488 share this code path but have not
// been tested; the HS+ in particular uses different endpoints and needs an
// extra init step the kernel driver calls `ni_usb_hs_plus_extra_init`, which is
// not implemented here.

pub mod protocol;
pub mod usb;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::backend::{GpibBackend, SharedBackend};
use protocol::*;

/// Backend identifier used by `--backend`.
pub const ID: &str = "ni-usb-hs";

/// Human-readable description shown by `--backend list`.
pub const DESCRIPTION: &str = "NI GPIB-USB-HS (HS+ / KUSB-488A / MC-USB-488 untested)";

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
}

/// GPIB-USB-HS controller. The controller's own primary address is fixed at 0;
/// `pad` arguments address the remote instrument.
pub struct NiUsbHsBackend<T: NiTransport> {
    transport: T,
    my_pad: u8,
    eos_char: u8,
    eos_enabled: bool,
    timeout_ms: u32,
}

impl<T: NiTransport> NiUsbHsBackend<T> {
    pub fn new(transport: T, timeout_ms: u32) -> Self {
        Self {
            transport,
            my_pad: 0,
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
        self.transport.bulk_out(req).await?;
        self.transport.bulk_in(resp_len).await
    }

    /// Take control (assert ATN), send command bytes, then optionally return to
    /// standby. Command bytes are capped at 16 per transfer by the hardware.
    async fn send_command(&self, cmd: &[u8], standby_after: bool) -> Result<()> {
        let tc = timeout_code(self.timeout_ms);
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
        // Readiness handshake: the adapter needs a moment after enumeration.
        usb::wait_for_ready(&self.transport).await?;
        // Monitor nothing while the chip is being reconfigured, per the kernel
        // driver's attach ordering.
        //
        // Unlike the kernel we then leave monitoring *off*. Enabling it makes
        // the adapter push an ibsta report onto its interrupt endpoint on every
        // status change; the kernel keeps an URB permanently submitted to drain
        // those, but this daemon is synchronous request/response and has no
        // reader. The backlog builds over a session and stalls the adapter's
        // bulk transfers on the next one — reliably hanging the first IFC after
        // a restart, recoverable only by physically replugging.
        //
        // If async SRQ notification is ever added, re-enable it *and* add a task
        // that continuously drains `interrupt_in_ep`; do not enable one without
        // the other. See `IBSTA_MONITOR_MASK` for the bits the kernel watches.
        // The HS+ wants three more vendor reads before it will talk GPIB.
        if self.transport.product_id() == usb::PID_NI_USB_HS_PLUS {
            usb::hs_plus_extra_init(&self.transport).await?;
        }
        usb::set_interrupt_monitor(&self.transport, 0).await?;
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

        info!("NI GPIB-USB-HS initialized at pad {my_pad}");
        Ok(())
    }

    async fn write(&mut self, pad: u8, data: &[u8], send_eoi: bool) -> Result<()> {
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
        // Address controller as listener (pad 0), instrument as talker.
        let cmd = [GPIB_UNL, listen_address(self.my_pad), talk_address(pad)];
        self.send_command(&cmd, true).await?;
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
        Ok(data.first().copied().unwrap_or(0))
    }

    /// Read the TNT4882 bus status register and report the live SRQ line.
    async fn srq_asserted(&mut self) -> Result<bool> {
        let vals = self
            .register_read(&[(SUBDEV_TNT4882, REG_BSR)])
            .await
            .context("ni bus status read")?;
        let bsr = *vals
            .first()
            .ok_or_else(|| anyhow::anyhow!("ni bus status read returned no data"))?;
        Ok(bsr & BCSR_SRQ != 0)
    }

    fn set_eos(&mut self, eos_char: u8, enabled: bool) {
        self.eos_char = eos_char;
        self.eos_enabled = enabled;
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
        vec![reg_write_ok(26), op_ok(), reg_write_ok(1)]
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

    #[tokio::test]
    async fn init_writes_registers_then_pulses_ifc_and_asserts_ren() {
        let t = MockTransport::new(init_responses());
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.init(0).await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        assert_eq!(writes.len(), 3, "init: register sequence, IFC, REN");

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
    async fn init_leaves_interrupt_monitoring_disabled() {
        // Enabling it without a reader lets ibsta reports pile up on the
        // interrupt endpoint and stalls the adapter on the next session.
        let monitor_masks = std::sync::Arc::new(Mutex::new(Vec::new()));
        struct Recorder(std::sync::Arc<Mutex<Vec<u16>>>, MockTransport);
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
                    self.0.lock().unwrap().push(i);
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
        let t = Recorder(monitor_masks.clone(), MockTransport::new(init_responses()));
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.init(0).await.unwrap();
        let masks = monitor_masks.lock().unwrap().clone();
        assert!(!masks.is_empty(), "monitoring should be explicitly set");
        assert!(
            masks.iter().all(|&m| m == 0),
            "every monitor mask must be 0, got {masks:?}"
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
