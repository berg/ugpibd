// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors

use crate::protocol::*;
use anyhow::Result;

#[allow(async_fn_in_trait)]
pub trait Transport {
    async fn write_bulk(&self, data: &[u8]) -> Result<()>;
    async fn read_bulk(&self, max_len: usize) -> Result<Vec<u8>>;
    /// Issue a vendor control-IN transfer (bmRequestType = 0xC0).
    async fn control_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        max_len: usize,
    ) -> Result<Vec<u8>>;
    /// Block until the device signals write-complete via the interrupt endpoint.
    /// On a mock transport, this returns immediately.
    async fn await_write_complete(&self) -> Result<()>;
}

pub struct GpibController<T: Transport> {
    pub transport: T,
    pub timeout_ms: u32,
    pub eos_char: u8,
    pub eos_enabled: bool,
    pub hw_control_bits: u8,
}

impl<T: Transport> GpibController<T> {
    pub fn new(transport: T, timeout_ms: u32) -> Self {
        Self {
            transport,
            timeout_ms,
            eos_char: b'\n',
            eos_enabled: false,
            hw_control_bits: 0,
        }
    }

    pub async fn write_registers(&mut self, regs: &[RegisterPairlet]) -> Result<()> {
        let pkt = encode_wr_regs(regs);
        self.transport.write_bulk(&pkt).await?;
        let resp = self.transport.read_bulk(0x20).await?;
        decode_wr_regs_response(&resp)?;
        Ok(())
    }

    pub async fn read_registers(&mut self, regs: &mut [RegisterPairlet]) -> Result<()> {
        let addrs: Vec<u8> = regs.iter().map(|r| r.address).collect();
        let pkt = encode_rd_regs(&addrs);
        self.transport.write_bulk(&pkt).await?;
        let resp = self.transport.read_bulk(0x20).await?;
        decode_rd_regs_response(&resp, regs)?;
        Ok(())
    }

    /// Initialize the GPIB controller. `my_pad` is our primary address (always 0).
    /// Matches the kernel driver's `agilent_82357a_init()` with t1_nano_sec=800,
    /// then requests system controller and pulses IFC + asserts REN.
    pub async fn init(&mut self, my_pad: u8) -> Result<()> {
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

    /// Send Selected Device Clear to `pad` (SDC = 0x04, preceded by addressing).
    pub async fn device_clear(&mut self, pad: u8) -> Result<()> {
        let cmd = [0x3f_u8, 0x40 + pad, 0x04]; // UNL, TAD(pad), SDC
        self.send_command_bytes(&cmd).await
    }

    /// Write `data` to instrument at `pad`. Handles GPIB addressing internally.
    pub async fn write(&mut self, pad: u8, data: &[u8], send_eoi: bool) -> Result<()> {
        // Address: UNT, MTA(0), LAD(pad)
        let addr_cmd = [0x5f_u8, 0x40_u8, 0x20 + pad];
        self.send_command_bytes(&addr_cmd).await?;
        self.send_data_bytes(data, send_eoi).await
    }

    /// Read up to `max_len` bytes from instrument at `pad`. Returns (data, end_of_message).
    pub async fn read(&mut self, pad: u8, max_len: usize) -> Result<(Vec<u8>, bool)> {
        // Address: UNL, MLA(0), TAD(pad)
        let addr_cmd = [0x3f_u8, 0x20_u8, 0x40 + pad];
        self.send_command_bytes(&addr_cmd).await?;
        // Go to standby (release ATN)
        let gts = [RegisterPairlet {
            address: TMS_AUXCR,
            value: AUX_GTS,
        }];
        self.write_registers(&gts).await?;
        // Issue read command
        let pkt = encode_gpib_read(max_len as u32, self.eos_enabled, self.eos_char);
        self.transport.write_bulk(&pkt).await?;
        let raw = self.transport.read_bulk(max_len + 1).await?;
        Ok(decode_gpib_read_response(&raw))
    }

    async fn send_command_bytes(&mut self, cmd: &[u8]) -> Result<()> {
        let pkt = encode_gpib_command(cmd);
        self.transport.write_bulk(&pkt).await?;
        self.transport.await_write_complete().await?;
        self.get_xfer_status().await?;
        Ok(())
    }

    async fn send_data_bytes(&mut self, data: &[u8], send_eoi: bool) -> Result<()> {
        let pkt = encode_gpib_write(data, send_eoi);
        self.transport.write_bulk(&pkt).await?;
        self.transport.await_write_complete().await?;
        self.get_xfer_status().await?;
        Ok(())
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
        let cmd_len =
            u32::from_le_bytes([writes[0][4], writes[0][5], writes[0][6], writes[0][7]]) as usize;
        let cmd_bytes = &writes[0][8..8 + cmd_len];
        assert!(cmd_bytes.contains(&0x5f), "cmd_bytes={cmd_bytes:?}"); // UNT
        assert!(cmd_bytes.contains(&0x40)); // MTA(0)
        assert!(cmd_bytes.contains(&(0x20 + 15))); // LAD(15)
        assert_eq!(writes[1][0], BulkCmd::Write as u8);
        let data_flags = writes[1][3];
        assert!(data_flags & WriteFlag::NoAddress as u8 != 0);
        assert!(data_flags & WriteFlag::SendEoi as u8 != 0);
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
        let cmd_len =
            u32::from_le_bytes([writes[0][4], writes[0][5], writes[0][6], writes[0][7]]) as usize;
        let cmd_bytes = &writes[0][8..8 + cmd_len];
        assert!(cmd_bytes.contains(&0x3f)); // UNL
        assert!(cmd_bytes.contains(&(0x40 + 7))); // TAD(7)
        assert!(cmd_bytes.contains(&0x04)); // SDC
    }
}
