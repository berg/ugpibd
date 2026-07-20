// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// National Instruments GPIB-USB-HS backend, translated from the Linux kernel
// `drivers/gpib/ni_usb/ni_usb_gpib.c`. Unlike the 82357B this adapter needs no
// firmware upload — it boots ready and performs a control-endpoint readiness
// handshake instead.
//
// EXPERIMENTAL: this backend is a faithful translation of the C reference and
// its packet encodings are unit-tested, but it has NOT been exercised on a
// physical NI adapter. Bring-up on real hardware is expected to be needed.

pub mod protocol;
pub mod usb;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::backend::{GpibBackend, SharedBackend};
use protocol::*;

/// Backend identifier used by `--backend`.
pub const ID: &str = "ni-usb-hs";

/// Human-readable description shown by `--backend list`.
pub const DESCRIPTION: &str = "NI GPIB-USB-HS (experimental, untested on hardware)";

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
        let regs = setup_init(my_pad, self.eos_mode());
        self.register_write(&regs)
            .await
            .context("ni init register sequence")?;
        info!("NI GPIB-USB-HS initialized at pad {my_pad}");
        Ok(())
    }

    async fn write(&mut self, pad: u8, data: &[u8], send_eoi: bool) -> Result<()> {
        // Address controller as talker (pad 0), instrument as listener.
        let cmd = [GPIB_UNL, talk_address(self.my_pad), listen_address(pad)];
        self.send_command(&cmd, true).await?;
        let resp = self
            .transact(
                &encode_data_write(data, send_eoi, timeout_code(self.timeout_ms)),
                OP_RESP_LEN,
            )
            .await?;
        parse_write_response(&resp, data.len()).context("ni data write")?;
        Ok(())
    }

    async fn read(&mut self, pad: u8, max_len: usize) -> Result<(Vec<u8>, bool)> {
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
}

/// Discover, open, and initialize an NI GPIB-USB-HS. `port` restricts the
/// search to the device at that USB port id.
pub async fn open(timeout_ms: u32, port: Option<&str>) -> Result<SharedBackend> {
    warn!("ni-usb-hs backend is EXPERIMENTAL and untested on hardware");
    let transport = usb::NiUsbTransport::open(timeout_ms, port).await?;
    let backend: SharedBackend = std::sync::Arc::new(tokio::sync::Mutex::new(NiUsbHsBackend::new(
        transport, timeout_ms,
    )));
    Ok(backend)
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
        async fn control_in(&self, _r: u8, _v: u16, _i: u16, _m: usize) -> Result<Vec<u8>> {
            Ok(vec![0u8; 16])
        }
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
    async fn init_sends_one_26_register_write() {
        let t = MockTransport::new(vec![reg_write_ok(26)]);
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.init(0).await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        assert_eq!(
            writes.len(),
            1,
            "init should be a single register-write bulk"
        );
        assert_eq!(writes[0][0], NIUSB_REG_WRITE_ID);
        assert_eq!(writes[0][1], 26, "26 register writes");
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
    async fn ren_writes_aux_command() {
        let t = MockTransport::new(vec![reg_write_ok(1)]);
        let mut be = NiUsbHsBackend::new(t, 3000);
        be.ren(true).await.unwrap();
        let writes = be.transport.written.lock().unwrap().clone();
        assert_eq!(writes[0][0], NIUSB_REG_WRITE_ID);
        assert_eq!(&writes[0][3..6], &[SUBDEV_TNT4882, REG_AUXMR, AUX_SREN]);
    }
}
