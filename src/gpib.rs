// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors

use anyhow::Result;
use crate::protocol::*;

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
    hw_control_bits: u8,
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
        async fn control_in(
            &self,
            _req: u8,
            _val: u16,
            _idx: u16,
            _max: usize,
        ) -> Result<Vec<u8>> {
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

    pub(crate) fn wr_regs_ok_response() -> Vec<u8> {
        vec![!(BulkCmd::WrRegs as u8), 0, 0, 0, 0, 0, 0, 0]
    }

    #[tokio::test]
    async fn write_registers_sends_correct_packet() {
        let t = MockTransport::new();
        t.push_response(wr_regs_ok_response());
        let mut ctrl = GpibController::new(t, 3000);
        let regs = &[RegisterPairlet { address: 0x0a, value: 0x01 }];
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
        let mut regs = vec![RegisterPairlet { address: 0x0a, value: 0 }];
        ctrl.read_registers(&mut regs).await.unwrap();
        assert_eq!(regs[0].value, 0x42);
    }
}
