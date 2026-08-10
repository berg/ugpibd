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

    /// The server's address, so a caller can open sibling connections (the
    /// abort channel, or a second core connection) to the same daemon.
    pub fn server_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.stream.get_ref().peer_addr()
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

    /// create_link with lockDevice set (RULE B.6.3.1).
    pub async fn create_link_locked(
        &mut self,
        device: &str,
        lock_timeout_ms: u32,
    ) -> Result<CreateLinkResp> {
        let parms = CreateLinkParms {
            client_id: std::process::id() as i32,
            lock_device: true,
            lock_timeout_ms,
            device: device.as_bytes().to_vec(),
        };
        let mut args = Vec::new();
        parms.encode(&mut args);
        let results = self.core(CREATE_LINK, &args).await?;
        Ok(CreateLinkResp::decode(&results)?)
    }

    pub async fn device_lock(
        &mut self,
        lid: i32,
        waitlock: bool,
        lock_timeout_ms: u32,
    ) -> Result<u32> {
        let mut args = Vec::new();
        DeviceLockParms {
            lid,
            flags: if waitlock { OP_FLAG_WAITLOCK } else { 0 },
            lock_timeout_ms,
        }
        .encode(&mut args);
        let results = self.core(DEVICE_LOCK, &args).await?;
        Ok(decode_device_error(&results)?)
    }

    pub async fn device_unlock(&mut self, lid: i32) -> Result<u32> {
        let mut args = Vec::new();
        xdr::put_i32(&mut args, lid);
        let results = self.core(DEVICE_UNLOCK, &args).await?;
        Ok(decode_device_error(&results)?)
    }

    /// Like device_write but carrying waitlock and a lock timeout, for
    /// exercising the I/O lock gate.
    pub async fn device_write_flags(
        &mut self,
        lid: i32,
        data: &[u8],
        flags: u32,
        lock_timeout_ms: u32,
    ) -> Result<DeviceWriteResp> {
        let parms = DeviceWriteParms {
            lid,
            io_timeout_ms: 0,
            lock_timeout_ms,
            flags,
            data: data.to_vec(),
        };
        let mut args = Vec::new();
        parms.encode(&mut args);
        let results = self.core(DEVICE_WRITE, &args).await?;
        Ok(DeviceWriteResp::decode(&results)?)
    }
}

impl Vxi11Client {
    pub async fn device_enable_srq(
        &mut self,
        lid: i32,
        enable: bool,
        handle: &[u8],
    ) -> Result<u32> {
        let mut args = Vec::new();
        DeviceEnableSrqParms {
            lid,
            enable,
            handle: handle.to_vec(),
        }
        .encode(&mut args);
        let results = self.core(DEVICE_ENABLE_SRQ, &args).await?;
        Ok(decode_device_error(&results)?)
    }

    pub async fn create_intr_chan(
        &mut self,
        host_addr: std::net::Ipv4Addr,
        host_port: u16,
        prog_num: u32,
        prog_vers: u32,
        prog_family: u32,
    ) -> Result<u32> {
        let mut args = Vec::new();
        DeviceRemoteFunc {
            host_addr: u32::from(host_addr),
            host_port,
            prog_num,
            prog_vers,
            prog_family,
        }
        .encode(&mut args);
        let results = self.core(CREATE_INTR_CHAN, &args).await?;
        Ok(decode_device_error(&results)?)
    }

    pub async fn destroy_intr_chan(&mut self) -> Result<u32> {
        let results = self.core(DESTROY_INTR_CHAN, &[]).await?;
        Ok(decode_device_error(&results)?)
    }

    pub async fn device_docmd(
        &mut self,
        lid: i32,
        cmd: i32,
        network_order: bool,
        datasize: i32,
        data_in: &[u8],
    ) -> Result<DeviceDocmdResp> {
        let parms = DeviceDocmdParms {
            lid,
            flags: 0,
            io_timeout_ms: 0,
            lock_timeout_ms: 0,
            cmd,
            network_order,
            datasize,
            data_in: data_in.to_vec(),
        };
        let mut args = Vec::new();
        parms.encode(&mut args);
        let results = self.core(DEVICE_DOCMD, &args).await?;
        Ok(DeviceDocmdResp::decode(&results)?)
    }
}

/// A DEVICE_INTR server: what a VXI-11 *client* runs so the instrument side
/// can call back with service requests. Serves the CLI's SRQ wait and the
/// test suite; accepts one connection at a time, answers each
/// device_intr_srq with the void reply the RPC layer owes, and hands the
/// received handles to the caller.
pub struct IntrServer {
    listener: tokio::net::TcpListener,
}

impl IntrServer {
    pub async fn bind() -> Result<Self> {
        Ok(Self {
            listener: tokio::net::TcpListener::bind("127.0.0.1:0").await?,
        })
    }

    pub fn port(&self) -> Result<u16> {
        Ok(self.listener.local_addr()?.port())
    }

    /// Serve connections forever, pushing each received handle into `tx`.
    pub async fn run(self, tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else {
                return;
            };
            let mut stream = BufStream::new(stream);
            loop {
                let record = match rpc::read_record(&mut stream, 4096).await {
                    Ok(Some(r)) => r,
                    _ => break,
                };
                let Ok((header, args)) = rpc::decode_call(&record) else {
                    break;
                };
                if header.prog != DEVICE_INTR_PROG || header.proc != DEVICE_INTR_SRQ {
                    let _ =
                        rpc::write_record(&mut stream, &rpc::reply_prog_unavail(header.xid)).await;
                    continue;
                }
                if let Ok(parms) = DeviceSrqParms::decode(args) {
                    let _ = tx.send(parms.handle);
                }
                // void reply: SUCCESS with no results.
                if rpc::write_record(&mut stream, &rpc::reply_success(header.xid, &[]))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// One-shot device_abort over its own connection to the abort channel —
/// which is the point: it must get through while the core channel is busy.
pub async fn device_abort(host: &str, abort_port: u16, lid: i32) -> Result<u32> {
    let mut client = Vxi11Client::connect(host, abort_port).await?;
    let mut args = Vec::new();
    xdr::put_i32(&mut args, lid);
    match client
        .call(DEVICE_ASYNC_PROG, DEVICE_ASYNC_VERS, DEVICE_ABORT, &args)
        .await?
    {
        Reply::Success(results) => Ok(decode_device_error(&results)?),
        other => anyhow::bail!("abort call failed at the RPC layer: {other:?}"),
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
