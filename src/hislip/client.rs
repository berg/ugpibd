// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// HiSLIP client. Drives the same message codec as the server (messages.rs)
// over two TCP channels: a synchronous channel for data/trigger and an
// asynchronous channel for device-clear / REN / status. Used by the `scpi`
// CLI.
//
// The server does not forward SRQ, so every server reply is solicited: each
// method here is a self-contained request/response round-trip and no
// background reader is needed.

use anyhow::{bail, Context, Result};
use byteorder::{ByteOrder, NetworkEndian};
use tokio::io::{BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use super::messages::{InitializeResponseParameter, Message, MessageType, RmtDeliveredControl};
use super::protocol::SUPPORTED_PROTOCOL;

/// First MessageID per HiSLIP spec; incremented by 2 per data transaction.
const FIRST_MESSAGE_ID: u32 = 0xffff_ff00;
/// Largest server reply we will accept for a single message.
const MAX_MESSAGE_SIZE: u64 = 16 * 1024 * 1024;

type Reader = BufReader<OwnedReadHalf>;
type Writer = BufWriter<OwnedWriteHalf>;

/// A connected HiSLIP session (sync + async channels).
pub struct HislipClient {
    sync_rd: Reader,
    sync_wr: Writer,
    async_rd: Reader,
    async_wr: Writer,
    message_id: u32,
}

impl HislipClient {
    /// Open both channels and complete the HiSLIP handshake.
    ///
    /// `subaddress` is the HiSLIP sub-address (e.g. `hislip14` or `hislip0`)
    /// that the server maps to a GPIB primary address.
    pub async fn connect(host: &str, port: u16, subaddress: &str, vendor_id: u16) -> Result<Self> {
        let target = format!("{host}:{port}");

        // --- Synchronous channel: Initialize -> InitializeResponse ---
        let sync = TcpStream::connect(&target)
            .await
            .with_context(|| format!("connect sync channel to {target}"))?;
        sync.set_nodelay(true).ok();
        let (srd, swr) = sync.into_split();
        let mut sync_rd = BufReader::new(srd);
        let mut sync_wr = BufWriter::new(swr);

        let init_param = SUPPORTED_PROTOCOL.as_parameter(vendor_id);
        MessageType::Initialize
            .message_params(0, init_param)
            .with_payload(subaddress.as_bytes().to_vec())
            .write_to(&mut sync_wr)
            .await?;
        flush(&mut sync_wr).await?;

        let resp = read_msg(&mut sync_rd)
            .await
            .context("read InitializeResponse")?;
        if resp.message_type != MessageType::InitializeResponse {
            bail!(
                "handshake failed: expected InitializeResponse, got {:?}: {}",
                resp.message_type,
                String::from_utf8_lossy(&resp.payload)
            );
        }
        let session_id = InitializeResponseParameter(resp.message_parameter).session_id();

        // --- Asynchronous channel: AsyncInitialize -> AsyncInitializeResponse ---
        let achan = TcpStream::connect(&target)
            .await
            .with_context(|| format!("connect async channel to {target}"))?;
        achan.set_nodelay(true).ok();
        let (ard, awr) = achan.into_split();
        let mut async_rd = BufReader::new(ard);
        let mut async_wr = BufWriter::new(awr);

        MessageType::AsyncInitialize
            .message_params(0, session_id as u32)
            .no_payload()
            .write_to(&mut async_wr)
            .await?;
        flush(&mut async_wr).await?;
        let resp = read_msg(&mut async_rd)
            .await
            .context("read AsyncInitializeResponse")?;
        if resp.message_type != MessageType::AsyncInitializeResponse {
            bail!(
                "handshake failed: expected AsyncInitializeResponse, got {:?}",
                resp.message_type
            );
        }

        // Advertise the largest reply we will accept so the server chunks
        // responses to fit.
        let mut buf = [0u8; 8];
        NetworkEndian::write_u64(&mut buf, MAX_MESSAGE_SIZE);
        MessageType::AsyncMaximumMessageSize
            .message_params(0, 0)
            .with_payload(buf.to_vec())
            .write_to(&mut async_wr)
            .await?;
        flush(&mut async_wr).await?;
        let resp = read_msg(&mut async_rd)
            .await
            .context("read AsyncMaximumMessageSizeResponse")?;
        if resp.message_type != MessageType::AsyncMaximumMessageSizeResponse {
            bail!(
                "handshake failed: expected AsyncMaximumMessageSizeResponse, got {:?}",
                resp.message_type
            );
        }

        Ok(Self {
            sync_rd,
            sync_wr,
            async_rd,
            async_wr,
            message_id: FIRST_MESSAGE_ID,
        })
    }

    /// Consume and return the next data MessageID, advancing the counter.
    fn next_message_id(&mut self) -> u32 {
        let id = self.message_id;
        self.message_id = self.message_id.wrapping_add(2);
        id
    }

    /// Send `cmd` as a single DataEnd message on the sync channel.
    async fn send_data(&mut self, cmd: &[u8], message_id: u32) -> Result<()> {
        MessageType::DataEnd
            .message_params(RmtDeliveredControl(0).0, message_id)
            .with_payload(cmd.to_vec())
            .write_to(&mut self.sync_wr)
            .await?;
        flush(&mut self.sync_wr).await?;
        Ok(())
    }

    /// Write `cmd` to the instrument without reading a response. Use for SCPI
    /// commands that do not produce output.
    pub async fn write(&mut self, cmd: &[u8]) -> Result<()> {
        let id = self.next_message_id();
        self.send_data(cmd, id).await
    }

    /// Send a GPIB trigger (GET) on the sync channel. No reply.
    pub async fn trigger(&mut self) -> Result<()> {
        let id = self.next_message_id();
        MessageType::Trigger
            .message_params(RmtDeliveredControl(0).0, id)
            .no_payload()
            .write_to(&mut self.sync_wr)
            .await?;
        flush(&mut self.sync_wr).await?;
        Ok(())
    }

    /// Send a single async request and read the expected response type.
    async fn async_request(&mut self, request: Message, expect: MessageType) -> Result<Message> {
        request.write_to(&mut self.async_wr).await?;
        flush(&mut self.async_wr).await?;
        let resp = read_msg(&mut self.async_rd)
            .await
            .context("read async reply")?;
        if resp.message_type != expect {
            bail!("expected {expect:?}, got {:?}", resp.message_type);
        }
        Ok(resp)
    }

    /// Send a Selected Device Clear and wait for the acknowledge.
    pub async fn clear(&mut self) -> Result<()> {
        let req = MessageType::AsyncDeviceClear
            .message_params(0, 0)
            .no_payload();
        self.async_request(req, MessageType::AsyncDeviceClearAcknowledge)
            .await?;
        Ok(())
    }

    /// Drive REN on (`true`) or off (`false`).
    pub async fn remote(&mut self, on: bool) -> Result<()> {
        // Control codes per HiSLIP §6.3: 3 enables remote, 0 disables it.
        // These match the server's set_remote mapping.
        let control = if on { 3 } else { 0 };
        let req = MessageType::AsyncRemoteLocalControl
            .message_params(control, 0)
            .no_payload();
        self.async_request(req, MessageType::AsyncRemoteLocalResponse)
            .await?;
        Ok(())
    }

    /// Read the instrument's serial-poll status byte.
    pub async fn status(&mut self) -> Result<u8> {
        let req = MessageType::AsyncStatusQuery
            .message_params(0, 0)
            .no_payload();
        let resp = self
            .async_request(req, MessageType::AsyncStatusResponse)
            .await?;
        Ok(resp.control_code)
    }

    /// Write `cmd` to the instrument and read back its response, returning the
    /// concatenated payload. Use for SCPI queries (commands containing `?`).
    pub async fn query(&mut self, cmd: &[u8]) -> Result<Vec<u8>> {
        let id = self.next_message_id();
        self.send_data(cmd, id).await?;

        let mut out = Vec::new();
        loop {
            let msg = read_msg(&mut self.sync_rd)
                .await
                .context("read query response")?;
            match msg.message_type {
                MessageType::Data | MessageType::DataEnd => {
                    if msg.message_parameter != id {
                        bail!(
                            "response MessageID {} does not match request {}",
                            msg.message_parameter,
                            id
                        );
                    }
                    out.extend_from_slice(&msg.payload);
                    if msg.message_type == MessageType::DataEnd {
                        return Ok(out);
                    }
                }
                MessageType::Error => {
                    bail!("device error: {}", String::from_utf8_lossy(&msg.payload));
                }
                MessageType::FatalError => {
                    bail!("fatal error: {}", String::from_utf8_lossy(&msg.payload));
                }
                other => bail!("unexpected sync reply: {other:?}"),
            }
        }
    }
}

/// Flush a buffered writer.
async fn flush(wr: &mut Writer) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    wr.flush().await?;
    Ok(())
}

/// Read one protocol message, mapping codec/protocol errors to `anyhow`.
async fn read_msg(rd: &mut Reader) -> Result<Message> {
    match Message::read_from(rd, MAX_MESSAGE_SIZE).await {
        Ok(Ok(m)) => Ok(m),
        Ok(Err(proto)) => Err(anyhow::anyhow!("protocol error: {proto}")),
        Err(io) => Err(anyhow::Error::from(io)).context("hislip read"),
    }
}
