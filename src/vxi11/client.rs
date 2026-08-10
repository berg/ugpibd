// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// VXI-11 core-channel client: one TCP connection, one call in flight.
// Serves the CLI and the test suite — the same wire code the server is
// tested against, mirroring how the HiSLIP client doubles as its server's
// proof.

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::BufStream;
use tokio::net::TcpStream;

use super::messages::*;
use super::rpc;
use super::xdr;
use super::*;

pub struct Vxi11Client {
    stream: BufStream<TcpStream>,
    xid: u32,
}

impl Vxi11Client {
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        let stream = TcpStream::connect((host, port))
            .await
            .with_context(|| format!("connect to VXI-11 server {host}:{port}"))?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream: BufStream::new(stream),
            xid: 1,
        })
    }

    /// One raw call: encode, send, await the matching reply. Exposed so
    /// tests can speak wrong programs and versions on purpose.
    pub async fn call(&mut self, prog: u32, vers: u32, proc: u32, args: &[u8]) -> Result<Reply> {
        self.xid = self.xid.wrapping_add(1);
        let record = rpc::encode_call(self.xid, prog, vers, proc, args);
        rpc::write_record(&mut self.stream, &record).await?;
        let reply = rpc::read_record(&mut self.stream, rpc::RECORD_MAX)
            .await?
            .ok_or_else(|| anyhow!("server closed the connection mid-call"))?;
        match rpc::decode_reply(&reply, self.xid).map_err(|e| anyhow!("{e}"))? {
            rpc::ReplyBody::Success(results) => Ok(Reply::Success(results.to_vec())),
            rpc::ReplyBody::Accepted(stat) => Ok(Reply::Accepted(stat)),
            rpc::ReplyBody::Denied(stat) => Ok(Reply::Denied(stat)),
        }
    }

    /// A core-channel call whose RPC layer must succeed; VXI-11-level
    /// errors stay in the returned bytes for the caller to decode.
    async fn core(&mut self, proc: u32, args: &[u8]) -> Result<Vec<u8>> {
        match self
            .call(DEVICE_CORE_PROG, DEVICE_CORE_VERS, proc, args)
            .await?
        {
            Reply::Success(results) => Ok(results),
            Reply::Accepted(stat) => bail!("RPC accepted-error {stat} on core procedure {proc}"),
            Reply::Denied(stat) => bail!("RPC denied ({stat}) on core procedure {proc}"),
        }
    }

    pub async fn create_link(&mut self, device: &str) -> Result<CreateLinkResp> {
        let parms = CreateLinkParms {
            client_id: std::process::id() as i32,
            lock_device: false,
            lock_timeout_ms: 0,
            device: device.as_bytes().to_vec(),
        };
        let mut args = Vec::new();
        parms.encode(&mut args);
        let results = self.core(CREATE_LINK, &args).await?;
        Ok(CreateLinkResp::decode(&results)?)
    }

    pub async fn device_write(
        &mut self,
        lid: i32,
        data: &[u8],
        end: bool,
        io_timeout_ms: u32,
    ) -> Result<DeviceWriteResp> {
        let parms = DeviceWriteParms {
            lid,
            io_timeout_ms,
            lock_timeout_ms: 0,
            flags: if end { OP_FLAG_END } else { 0 },
            data: data.to_vec(),
        };
        let mut args = Vec::new();
        parms.encode(&mut args);
        let results = self.core(DEVICE_WRITE, &args).await?;
        Ok(DeviceWriteResp::decode(&results)?)
    }

    pub async fn device_read(
        &mut self,
        lid: i32,
        request_size: u32,
        io_timeout_ms: u32,
        term_char: Option<u8>,
    ) -> Result<DeviceReadResp> {
        let parms = DeviceReadParms {
            lid,
            request_size,
            io_timeout_ms,
            lock_timeout_ms: 0,
            flags: term_char.map_or(0, |_| OP_FLAG_TERMCHRSET),
            term_char: term_char.unwrap_or(0) as i32,
        };
        let mut args = Vec::new();
        parms.encode(&mut args);
        let results = self.core(DEVICE_READ, &args).await?;
        Ok(DeviceReadResp::decode(&results, u32::MAX)?)
    }

    pub async fn device_readstb(
        &mut self,
        lid: i32,
        io_timeout_ms: u32,
    ) -> Result<DeviceReadStbResp> {
        let results = self
            .core(DEVICE_READSTB, &generic(lid, io_timeout_ms))
            .await?;
        Ok(DeviceReadStbResp::decode(&results)?)
    }

    pub async fn device_trigger(&mut self, lid: i32) -> Result<u32> {
        let results = self.core(DEVICE_TRIGGER, &generic(lid, 0)).await?;
        Ok(decode_device_error(&results)?)
    }

    pub async fn device_clear(&mut self, lid: i32) -> Result<u32> {
        let results = self.core(DEVICE_CLEAR, &generic(lid, 0)).await?;
        Ok(decode_device_error(&results)?)
    }

    pub async fn device_remote(&mut self, lid: i32) -> Result<u32> {
        let results = self.core(DEVICE_REMOTE, &generic(lid, 0)).await?;
        Ok(decode_device_error(&results)?)
    }

    pub async fn device_local(&mut self, lid: i32) -> Result<u32> {
        let results = self.core(DEVICE_LOCAL, &generic(lid, 0)).await?;
        Ok(decode_device_error(&results)?)
    }

    pub async fn destroy_link(&mut self, lid: i32) -> Result<u32> {
        let mut args = Vec::new();
        xdr::put_i32(&mut args, lid);
        let results = self.core(DESTROY_LINK, &args).await?;
        Ok(decode_device_error(&results)?)
    }
}

/// A decoded RPC-level outcome; owned, unlike `rpc::ReplyBody`, so callers
/// can hold it across further calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Success(Vec<u8>),
    Accepted(u32),
    Denied(u32),
}

fn generic(lid: i32, io_timeout_ms: u32) -> Vec<u8> {
    let mut args = Vec::new();
    DeviceGenericParms {
        lid,
        flags: 0,
        lock_timeout_ms: 0,
        io_timeout_ms,
    }
    .encode(&mut args);
    args
}
