// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors
//
// HiSLIP server. Accepts one TCP connection per session-channel. Each client
// opens two connections to the same port: a synchronous channel for
// data/trigger, and an asynchronous channel for lock/clear/status/REN. They
// are paired by the 16-bit session id the server hands out on the sync
// Initialize, which the client echoes on AsyncInitialize.
//
// For single-bus / single-instrument deployments we skip the full locking
// and multi-device machinery of the spec. Lock requests always succeed
// (there is only ever one client competing for one bus), SRQ is not
// forwarded, and TLS/SASL handshakes are rejected.

use std::collections::HashMap;
use std::io;
use std::str::from_utf8;
use std::sync::Arc;

use anyhow::Result;
use byteorder::{ByteOrder, NetworkEndian};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::errors::{FatalErrorCode, NonFatalErrorCode};
use super::messages::{
    send_fatal, send_nonfatal, AsyncInitializeResponseControl, AsyncInitializeResponseParameter,
    FeatureBitmap, InitializeParameter, InitializeResponseControl, InitializeResponseParameter,
    Message, MessageType, ReleaseLockControl, RequestLockControl,
};
use super::protocol::{Protocol, SUPPORTED_PROTOCOL};
use super::DEFAULT_SUBADDRESS;

/// A HiSLIP device endpoint. The server resolves a subaddress to one of
/// these on Initialize and then drives all per-session I/O through it.
///
/// All methods are cancel-safe at the GPIB-bus level: the underlying
/// [`crate::gpib::GpibController`] serializes calls via its own mutex.
#[async_trait::async_trait]
pub trait Device: Send + Sync + 'static {
    /// Execute a full query: write `cmd` to the instrument with EOI on the
    /// last byte, then (if `expect_response`) read a response.
    async fn execute(&self, cmd: &[u8], expect_response: bool) -> Result<Option<Vec<u8>>>;

    /// Send a GPIB trigger (GET) to the instrument.
    async fn trigger(&self) -> Result<()>;

    /// Send Selected Device Clear to the instrument.
    async fn clear(&self) -> Result<()>;

    /// Drive REN on/off.
    async fn set_remote(&self, remote: bool) -> Result<()>;

    /// Read serial-poll status byte. Return 0 if unsupported.
    async fn get_status(&self) -> u8 {
        0
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub vendor_id: u16,
    pub max_message_size: u64,
    pub max_sessions: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vendor_id: 0xBEEF,
            max_message_size: 1024 * 1024,
            max_sessions: 16,
        }
    }
}

/// Server entry point. `device_for` resolves a subaddress string (e.g.
/// "hislip0", "gpib0,14") to a boxed [`Device`]. Returning `None` causes
/// the connection to be rejected with InvalidInitialization.
pub async fn run<F>(listener: TcpListener, config: Config, device_for: F) -> io::Result<()>
where
    F: Fn(&str) -> Option<Arc<dyn Device>> + Send + Sync + 'static,
{
    info!("HiSLIP listening on {}", listener.local_addr()?);
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    let device_for = Arc::new(device_for);

    loop {
        let (stream, addr) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let registry = registry.clone();
        let config = config.clone();
        let device_for = device_for.clone();
        tokio::spawn(async move {
            info!(%addr, "hislip client connected");
            if let Err(e) = handle_connection(stream, config, registry, device_for).await {
                warn!(%addr, "hislip client error: {e:#}");
            } else {
                info!(%addr, "hislip client disconnected");
            }
        });
    }
}

type Registry = Arc<Mutex<HashMap<u16, Arc<SessionEntry>>>>;

struct SessionEntry {
    protocol: Protocol,
    /// `max_message_size` the client says it can receive; chunked response
    /// writes on the sync channel respect this.
    client_max_message_size: Mutex<u64>,
    device: Arc<dyn Device>,
    /// Tracks whether an async channel has already bound to this session.
    async_bound: Mutex<bool>,
}

async fn handle_connection<F>(
    stream: TcpStream,
    config: Config,
    registry: Registry,
    device_for: Arc<F>,
) -> io::Result<()>
where
    F: Fn(&str) -> Option<Arc<dyn Device>> + Send + Sync + 'static,
{
    let (rd, wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let mut wr = BufWriter::new(wr);

    // A fresh connection must begin with either Initialize (sync channel) or
    // AsyncInitialize (async channel). We read one message and dispatch.
    let first = match Message::read_from(&mut rd, config.max_message_size).await? {
        Ok(m) => m,
        Err(e) => {
            Message::from(e).write_to(&mut wr).await?;
            wr.flush().await?;
            return Ok(());
        }
    };

    match first.message_type {
        MessageType::Initialize => init_sync(first, rd, wr, config, registry, device_for).await,
        MessageType::AsyncInitialize => init_async(first, rd, wr, config, registry).await,
        other => {
            send_fatal(
                &mut wr,
                FatalErrorCode::InvalidInitialization,
                format!("first message must be (Async)Initialize, got {other:?}"),
            )
            .await?;
            Ok(())
        }
    }
}

async fn init_sync<R, W, F>(
    init: Message,
    mut rd: R,
    mut wr: W,
    config: Config,
    registry: Registry,
    device_for: Arc<F>,
) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
    F: Fn(&str) -> Option<Arc<dyn Device>> + Send + Sync + 'static,
{
    let params = InitializeParameter(init.message_parameter);
    let client_protocol = params.client_protocol();
    let protocol = std::cmp::min(SUPPORTED_PROTOCOL, client_protocol);

    let subaddr = match String::from_utf8(init.payload) {
        Ok(s) if s.is_empty() => DEFAULT_SUBADDRESS.to_string(),
        Ok(s) => s,
        Err(_) => {
            send_fatal(
                &mut wr,
                FatalErrorCode::InvalidInitialization,
                "subaddress is not valid UTF-8",
            )
            .await?;
            return Ok(());
        }
    };

    let device = match device_for(&subaddr) {
        Some(d) => d,
        None => {
            send_fatal(
                &mut wr,
                FatalErrorCode::InvalidInitialization,
                format!("unknown subaddress: {subaddr}"),
            )
            .await?;
            return Ok(());
        }
    };

    // Allocate a session id and register. Session ids are 16-bit, we use
    // even numbers and wrap; collisions would only happen with > 32k
    // concurrent clients.
    let entry = Arc::new(SessionEntry {
        protocol,
        client_max_message_size: Mutex::new(config.max_message_size),
        device: device.clone(),
        async_bound: Mutex::new(false),
    });
    let session_id = {
        let mut reg = registry.lock().await;
        if reg.len() >= config.max_sessions {
            drop(reg);
            send_fatal(
                &mut wr,
                FatalErrorCode::MaximumClientsExceeded,
                "too many active sessions",
            )
            .await?;
            return Ok(());
        }
        let mut id: u16 = 0;
        while reg.contains_key(&id) {
            id = id.wrapping_add(2);
            if id == 0 {
                drop(reg);
                send_fatal(
                    &mut wr,
                    FatalErrorCode::MaximumClientsExceeded,
                    "out of session ids",
                )
                .await?;
                return Ok(());
            }
        }
        reg.insert(id, entry.clone());
        id
    };

    debug!(session_id, %subaddr, %protocol, "hislip sync initialized");

    let resp_param = InitializeResponseParameter::new(protocol, session_id);
    let resp_ctrl = InitializeResponseControl::new(true, false, false);
    MessageType::InitializeResponse
        .message_params(resp_ctrl.0, resp_param.0)
        .no_payload()
        .write_to(&mut wr)
        .await?;
    wr.flush().await?;

    let guard = RegistrationGuard {
        id: session_id,
        registry: registry.clone(),
    };
    let result = sync_loop(&mut rd, &mut wr, entry, config).await;
    drop(guard);
    result
}

async fn init_async<R, W>(
    init: Message,
    mut rd: R,
    mut wr: W,
    config: Config,
    registry: Registry,
) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let session_id = (init.message_parameter & 0xFFFF) as u16;
    let entry = {
        let reg = registry.lock().await;
        reg.get(&session_id).cloned()
    };
    let entry = match entry {
        Some(e) => e,
        None => {
            send_fatal(
                &mut wr,
                FatalErrorCode::InvalidInitialization,
                format!("unknown session id {session_id}"),
            )
            .await?;
            return Ok(());
        }
    };

    {
        let mut bound = entry.async_bound.lock().await;
        if *bound {
            drop(bound);
            send_fatal(
                &mut wr,
                FatalErrorCode::InvalidInitialization,
                "async channel already bound for this session",
            )
            .await?;
            return Ok(());
        }
        *bound = true;
    }

    debug!(session_id, "hislip async initialized");

    let resp_ctrl = AsyncInitializeResponseControl::new(false);
    let resp_param = AsyncInitializeResponseParameter::new(config.vendor_id);
    MessageType::AsyncInitializeResponse
        .message_params(resp_ctrl.0, resp_param.0)
        .no_payload()
        .write_to(&mut wr)
        .await?;
    wr.flush().await?;

    async_loop(&mut rd, &mut wr, entry, config).await
}

struct RegistrationGuard {
    id: u16,
    registry: Registry,
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        let id = self.id;
        let registry = self.registry.clone();
        // Remove the entry in a detached task; Drop is sync but the mutex
        // is async. `try_lock` would race with a concurrent async-channel
        // init, so spawn a cleanup instead.
        tokio::spawn(async move {
            registry.lock().await.remove(&id);
        });
    }
}

async fn sync_loop<R, W>(
    rd: &mut R,
    wr: &mut W,
    entry: Arc<SessionEntry>,
    config: Config,
) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let msg = match Message::read_from(rd, config.max_message_size).await? {
            Ok(m) => m,
            Err(e) => {
                let fatal = e.is_fatal();
                Message::from(e).write_to(wr).await?;
                wr.flush().await?;
                if fatal {
                    return Ok(());
                }
                continue;
            }
        };

        match msg.message_type {
            MessageType::Data | MessageType::DataEnd => {
                let is_end = msg.message_type == MessageType::DataEnd;
                buffer.extend_from_slice(&msg.payload);
                if !is_end {
                    continue;
                }
                let cmd = std::mem::take(&mut buffer);
                let expect_response = cmd.contains(&b'?');
                let resp = match entry.device.execute(&cmd, expect_response).await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("device execute failed: {e:#}");
                        send_nonfatal(
                            wr,
                            NonFatalErrorCode::UnidentifiedError,
                            format!("device error: {e}"),
                        )
                        .await?;
                        continue;
                    }
                };
                if let Some(data) = resp {
                    let client_max = *entry.client_max_message_size.lock().await;
                    let max = std::cmp::max(256, client_max as usize);
                    let mut chunks = data.chunks(max).peekable();
                    while let Some(chunk) = chunks.next() {
                        let ty = if chunks.peek().is_none() {
                            MessageType::DataEnd
                        } else {
                            MessageType::Data
                        };
                        ty.message_params(0, msg.message_parameter)
                            .with_payload(chunk.to_vec())
                            .write_to(wr)
                            .await?;
                    }
                    wr.flush().await?;
                }
            }
            MessageType::Trigger => {
                if let Err(e) = entry.device.trigger().await {
                    warn!("trigger failed: {e:#}");
                }
            }
            MessageType::DeviceClearComplete => {
                // Client ack of clear — finish the handshake.
                let features = FeatureBitmap(msg.control_code);
                let agreed = FeatureBitmap::new(features.overlapped(), false, false);
                MessageType::DeviceClearAcknowledge
                    .message_params(agreed.0, 0)
                    .no_payload()
                    .write_to(wr)
                    .await?;
                wr.flush().await?;
            }
            MessageType::FatalError => {
                warn!(
                    "client fatal: {:?}",
                    from_utf8(&msg.payload).unwrap_or("<non-utf8>")
                );
                return Ok(());
            }
            MessageType::Error => {
                warn!(
                    "client non-fatal: {:?}",
                    from_utf8(&msg.payload).unwrap_or("<non-utf8>")
                );
            }
            MessageType::StartTLS
            | MessageType::EndTLS
            | MessageType::GetSaslMechanismList
            | MessageType::AuthenticationStart
            | MessageType::AuthenticationExchange
                if entry.protocol >= super::protocol::PROTOCOL_2_0 =>
            {
                send_fatal(
                    wr,
                    FatalErrorCode::SecureConnectionFailed,
                    "TLS/SASL not supported",
                )
                .await?;
                return Ok(());
            }
            other => {
                send_nonfatal(
                    wr,
                    NonFatalErrorCode::UnrecognizedMessageType,
                    format!("unexpected sync message: {other:?}"),
                )
                .await?;
            }
        }
    }
}

async fn async_loop<R, W>(
    rd: &mut R,
    wr: &mut W,
    entry: Arc<SessionEntry>,
    config: Config,
) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let msg = match Message::read_from(rd, config.max_message_size).await? {
            Ok(m) => m,
            Err(e) => {
                let fatal = e.is_fatal();
                Message::from(e).write_to(wr).await?;
                wr.flush().await?;
                if fatal {
                    return Ok(());
                }
                continue;
            }
        };

        match msg.message_type {
            MessageType::AsyncLock => {
                // Single-bus: locking is a no-op. Acknowledge as success.
                let control = if msg.control_code == 0 {
                    ReleaseLockControl::SuccessExclusive as u8
                } else {
                    RequestLockControl::Success as u8
                };
                MessageType::AsyncLockResponse
                    .message_params(control, 0)
                    .no_payload()
                    .write_to(wr)
                    .await?;
                wr.flush().await?;
            }
            MessageType::AsyncLockInfo => {
                // exclusive=0, shared_count=0
                MessageType::AsyncLockInfoResponse
                    .message_params(0, 0)
                    .no_payload()
                    .write_to(wr)
                    .await?;
                wr.flush().await?;
            }
            MessageType::AsyncMaximumMessageSize => {
                if msg.payload.len() != 8 {
                    send_fatal(
                        wr,
                        FatalErrorCode::PoorlyFormattedMessageHeader,
                        "AsyncMaximumMessageSize payload must be 8 bytes",
                    )
                    .await?;
                    return Ok(());
                }
                let size = NetworkEndian::read_u64(&msg.payload);
                *entry.client_max_message_size.lock().await = size;
                let mut buf = [0u8; 8];
                NetworkEndian::write_u64(&mut buf, config.max_message_size);
                MessageType::AsyncMaximumMessageSizeResponse
                    .message_params(0, 0)
                    .with_payload(buf.to_vec())
                    .write_to(wr)
                    .await?;
                wr.flush().await?;
            }
            MessageType::AsyncDeviceClear => {
                // The GpibController serializes ops across the shared
                // Arc<Mutex<_>>, so this clear is naturally ordered after
                // whatever the sync side is mid-doing — no explicit
                // cross-channel signal needed.
                if let Err(e) = entry.device.clear().await {
                    warn!("device clear failed: {e:#}");
                }
                let features = FeatureBitmap::new(true, false, false);
                MessageType::AsyncDeviceClearAcknowledge
                    .message_params(features.0, 0)
                    .no_payload()
                    .write_to(wr)
                    .await?;
                wr.flush().await?;
            }
            MessageType::AsyncRemoteLocalControl => {
                let res = match msg.control_code {
                    0 | 2 | 6 => entry.device.set_remote(false).await,
                    1 => Ok(()),
                    3..=5 => entry.device.set_remote(true).await,
                    _ => {
                        send_nonfatal(
                            wr,
                            NonFatalErrorCode::UnrecognizedControlCode,
                            format!("unknown remote/local code {}", msg.control_code),
                        )
                        .await?;
                        continue;
                    }
                };
                if let Err(e) = res {
                    warn!("set_remote failed: {e:#}");
                }
                MessageType::AsyncRemoteLocalResponse
                    .message_params(0, 0)
                    .no_payload()
                    .write_to(wr)
                    .await?;
                wr.flush().await?;
            }
            MessageType::AsyncStatusQuery => {
                let stb = entry.device.get_status().await;
                MessageType::AsyncStatusResponse
                    .message_params(stb, 0)
                    .no_payload()
                    .write_to(wr)
                    .await?;
                wr.flush().await?;
            }
            MessageType::AsyncStartTLS
            | MessageType::AsyncEndTLS
                if entry.protocol >= super::protocol::PROTOCOL_2_0 =>
            {
                send_fatal(
                    wr,
                    FatalErrorCode::SecureConnectionFailed,
                    "TLS not supported",
                )
                .await?;
                return Ok(());
            }
            MessageType::FatalError => {
                warn!(
                    "client fatal (async): {:?}",
                    from_utf8(&msg.payload).unwrap_or("<non-utf8>")
                );
                return Ok(());
            }
            MessageType::Error => {
                warn!(
                    "client non-fatal (async): {:?}",
                    from_utf8(&msg.payload).unwrap_or("<non-utf8>")
                );
            }
            other => {
                send_nonfatal(
                    wr,
                    NonFatalErrorCode::UnrecognizedMessageType,
                    format!("unexpected async message: {other:?}"),
                )
                .await?;
            }
        }
    }
}

/// Parse a HiSLIP subaddress (e.g. "hislip0", "gpib0,14", "14") into an
/// optional GPIB primary address. Returns `None` if no numeric PAD is
/// embedded in the string.
pub fn parse_subaddress_pad(sub: &str) -> Option<u8> {
    let s = sub.trim().trim_end_matches('\0');
    let tail = s.rsplit([',', ':']).next().unwrap_or("");
    if let Ok(n) = tail.parse::<u8>() {
        if n <= 30 {
            return Some(n);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pad() {
        assert_eq!(parse_subaddress_pad("gpib0,14"), Some(14));
        assert_eq!(parse_subaddress_pad("hislip0,7"), Some(7));
        assert_eq!(parse_subaddress_pad("14"), Some(14));
        assert_eq!(parse_subaddress_pad("hislip0"), None);
        assert_eq!(parse_subaddress_pad("gpib0,31"), None); // out of range
        assert_eq!(parse_subaddress_pad(""), None);
    }

    #[test]
    fn standard_port_is_4880() {
        assert_eq!(super::super::STANDARD_PORT, 4880);
    }
}
