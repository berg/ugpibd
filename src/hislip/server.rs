// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// HiSLIP server. Accepts one TCP connection per session-channel. Each client
// opens two connections to the same port: a synchronous channel for
// data/trigger, and an asynchronous channel for lock/clear/status/REN. They
// are paired by the 16-bit session id the server hands out on the sync
// Initialize, which the client echoes on AsyncInitialize.
//
// Locks are real: AsyncLock/AsyncLockInfo are backed by lock.rs and enforced
// against Data/Trigger/device-clear traffic from sessions that do not hold
// them. TLS/SASL handshakes are rejected. Multi-device is out of scope: one
// bus, addressed per session by the hislip<N> sub-address.
//
// Service requests reach the client from two places. When the adapter reports
// SRQ while the bus is idle, a per-session task serial-polls the device and
// pushes an AsyncServiceRequest carrying the status byte. When one is raised by
// a command this session is running, `Device::execute` reports it — see
// hislip/instrument.rs for why it has to be noticed there. Those are the only
// messages the server sends unsolicited, so they are also the only ones a
// client must be prepared to see mid-round-trip.

use std::collections::HashMap;
use std::io;
use std::str::from_utf8;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use byteorder::{ByteOrder, NetworkEndian};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::errors::{FatalErrorCode, NonFatalErrorCode};
use super::lock::LockRegistry;
use super::messages::{
    send_fatal, send_nonfatal, AsyncInitializeResponseControl, AsyncInitializeResponseParameter,
    FeatureBitmap, InitializeParameter, InitializeResponseControl, InitializeResponseParameter,
    Message, MessageType,
};
use super::protocol::{Protocol, SUPPORTED_PROTOCOL};
use super::DEFAULT_SUBADDRESS;

/// What running one command produced.
#[derive(Debug, Default)]
pub struct Execution {
    /// The instrument's reply, or `None` for a command we did not read back.
    pub data: Option<Vec<u8>>,
    /// Status byte of a service request the instrument raised while this
    /// command was running, if the device noticed one. Carried out here
    /// because the window it lives in is inside `execute` — see
    /// [`crate::hislip::instrument::GpibInstrument`].
    pub service_request: Option<u8>,
}

impl From<Option<Vec<u8>>> for Execution {
    fn from(data: Option<Vec<u8>>) -> Self {
        Self {
            data,
            service_request: None,
        }
    }
}

/// A HiSLIP device endpoint. The server resolves a subaddress to one of
/// these on Initialize and then drives all per-session I/O through it.
///
/// All methods are cancel-safe at the GPIB-bus level: the underlying
/// [`crate::backend::agilent_82357::gpib::GpibController`] serializes calls via its own mutex.
#[async_trait::async_trait]
pub trait Device: Send + Sync + 'static {
    /// Execute a full query: write `cmd` to the instrument with EOI on the
    /// last byte, then (if `expect_response`) read a response.
    async fn execute(&self, cmd: &[u8], expect_response: bool) -> Result<Execution>;

    /// Send a GPIB trigger (GET) to the instrument.
    async fn trigger(&self) -> Result<()>;

    /// Send Selected Device Clear to the instrument.
    async fn clear(&self) -> Result<()>;

    /// Drive REN on/off.
    async fn set_remote(&self, remote: bool) -> Result<()>;

    /// Read the instrument's serial-poll status byte.
    ///
    /// No default: 0 is a meaningful status ("nothing to report"), so an
    /// implementation that has no way to poll must return an error rather than
    /// a value the caller cannot distinguish from a real reading.
    async fn get_status(&self) -> Result<u8>;

    /// Subscribe to service requests raised by this device, so the session can
    /// forward them to the client as `AsyncServiceRequest`. `None` means the
    /// underlying adapter has no way to report SRQ, and the session simply
    /// never pushes one.
    async fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        None
    }

    /// Whether any device on the bus is currently asserting SRQ — a level read,
    /// not an event.
    ///
    /// Errors when the adapter cannot tell. Callers must not read that as "no
    /// SRQ": a fabricated no is indistinguishable from a quiet bus.
    async fn srq_asserted(&self) -> Result<bool> {
        anyhow::bail!("this device cannot read the SRQ line")
    }

    /// Key identifying the physical resource behind this device, used to scope
    /// locks. Two sessions that reach the same instrument must return the same
    /// key, and two that reach different instruments must not: locking the DMM
    /// has no business locking out the counter on the same bus.
    fn resource_key(&self) -> String {
        "default".to_string()
    }
}

/// IEEE-488 status byte bit 6: the device is requesting service. Set in the
/// byte returned by a serial poll of whichever device pulled SRQ.
pub const STB_RQS: u8 = 0x40;

/// IEEE-488 status byte bit 4: message available — the device has output
/// queued that nobody has read yet.
pub const STB_MAV: u8 = 0x10;

/// Non-fatal error code for "somebody else holds the lock". The defined codes
/// stop at 5 and 6..=127 are reserved by the spec, so this lands in the
/// device-defined range where a server is allowed to invent one.
const ERROR_LOCKED: u8 = 128;

/// How long the SRQ forwarder waits for the async channel's write lock before
/// saying so. Comfortably longer than any legitimate hold — the async loop only
/// holds it to service one request — so this firing means something is wedged.
/// It only logs; the forwarder keeps waiting.
const WRITE_LOCK_STUCK_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to keep re-polling this session's device while the SRQ line stays
/// asserted. Bounded because the line can legitimately stay low indefinitely —
/// a device with no session bound to it can request service that nothing will
/// ever clear, and that must not become a busy loop.
const SRQ_RECHECK_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// Gap between those re-polls. Each one is a bus transaction, so this trades
/// how fast an overlapping request is noticed against bus traffic.
const SRQ_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

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
    let locks = Arc::new(LockRegistry::new());
    let device_for = Arc::new(device_for);

    loop {
        let (stream, addr) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let registry = registry.clone();
        let locks = locks.clone();
        let config = config.clone();
        let device_for = device_for.clone();
        tokio::spawn(async move {
            info!(%addr, "hislip client connected");
            if let Err(e) = handle_connection(stream, config, registry, locks, device_for).await {
                warn!(%addr, "hislip client error: {e:#}");
            } else {
                info!(%addr, "hislip client disconnected");
            }
        });
    }
}

type Registry = Arc<Mutex<HashMap<u16, Arc<SessionEntry>>>>;

struct SessionEntry {
    /// This session's id — also how the lock registry identifies it.
    id: u16,
    protocol: Protocol,
    /// `max_message_size` the client says it can receive; chunked response
    /// writes on the sync channel respect this.
    client_max_message_size: Mutex<u64>,
    device: Arc<dyn Device>,
    /// Tracks whether an async channel has already bound to this session.
    async_bound: Mutex<bool>,
    /// Server-wide lock state, and the resource key this session locks under.
    locks: Arc<LockRegistry>,
    resource: String,
    /// The last status byte the daemon consumed on this client's behalf.
    ///
    /// A serial poll clears RQS at the instrument, so once the daemon has
    /// polled — which it must, to put a status byte in `AsyncServiceRequest` —
    /// the client's own `AsyncStatusQuery` would read a bit that has already
    /// been taken. Hold it and hand it over once. pyvisa-py hit the mirror
    /// image of this in VXI-11, where polling inside the SRQ handler broke
    /// because the bit was not cached.
    consumed_stb: std::sync::Mutex<Option<u8>>,
    /// Bumped by every device clear. The sync loop compares it across an
    /// operation to notice that its in-flight message was abandoned.
    clear_epoch: AtomicU64,
    /// Service requests the server raises itself, from the sync loop to
    /// whichever task owns the async channel's writer.
    srq_tx: mpsc::Sender<u8>,
    srq_rx: Mutex<Option<mpsc::Receiver<u8>>>,
}

/// Depth of the self-raised service request queue. A service request is a
/// level, not a count — a client that has not drained one does not need the
/// next four — so a small queue that drops on overflow loses nothing.
const SRQ_QUEUE_DEPTH: usize = 4;

impl SessionEntry {
    fn new(
        id: u16,
        protocol: Protocol,
        device: Arc<dyn Device>,
        locks: Arc<LockRegistry>,
        max_message_size: u64,
    ) -> Self {
        let (srq_tx, srq_rx) = mpsc::channel(SRQ_QUEUE_DEPTH);
        Self {
            id,
            protocol,
            client_max_message_size: Mutex::new(max_message_size),
            resource: device.resource_key(),
            device,
            async_bound: Mutex::new(false),
            locks,
            consumed_stb: std::sync::Mutex::new(None),
            clear_epoch: AtomicU64::new(0),
            srq_tx,
            srq_rx: Mutex::new(Some(srq_rx)),
        }
    }

    /// May this session do I/O right now, or is somebody else holding a lock?
    fn has_access(&self) -> bool {
        self.locks.has_access(&self.resource, self.id)
    }

    /// Push a service request the daemon observed on this session's behalf,
    /// remembering the status byte for the client's next `AsyncStatusQuery`.
    /// Dropped if no async channel is bound or the client is not keeping up —
    /// see [`SRQ_QUEUE_DEPTH`].
    fn raise_service_request(&self, stb: u8) {
        *self.consumed_stb.lock().unwrap() = Some(stb);
        if self.srq_tx.try_send(stb).is_err() {
            debug!(stb, "no async channel to raise a service request on");
        }
    }

    /// Take the status byte the daemon consumed, if it has not been handed
    /// over yet.
    fn take_consumed_stb(&self) -> Option<u8> {
        self.consumed_stb.lock().unwrap().take()
    }
}

async fn handle_connection<F>(
    stream: TcpStream,
    config: Config,
    registry: Registry,
    locks: Arc<LockRegistry>,
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
        MessageType::Initialize => {
            init_sync(first, rd, wr, config, registry, locks, device_for).await
        }
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
    locks: Arc<LockRegistry>,
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
    let (session_id, entry) = {
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
        // Built under the registry lock, so the id it carries cannot be handed
        // to a second session in between.
        let entry = Arc::new(SessionEntry::new(
            id,
            protocol,
            device.clone(),
            locks.clone(),
            config.max_message_size,
        ));
        reg.insert(id, entry.clone());
        (id, entry)
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
        locks,
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
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
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

    async_loop(&mut rd, wr, entry, config).await
}

struct RegistrationGuard {
    id: u16,
    registry: Registry,
    locks: Arc<LockRegistry>,
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        // Locks go first, and synchronously: a client that crashes holding one
        // would otherwise lock the instrument out until the daemon restarts.
        // The lock registry uses a std mutex precisely so this works from Drop.
        self.locks.release_all(self.id);

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
    // Snapshot of the device-clear counter, so an interrupted message can be
    // told apart from a completed one.
    let mut epoch = entry.clear_epoch.load(Ordering::Acquire);
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

        // A device clear since the last message means any half-assembled one
        // is void; the client will start again after the handshake.
        let current = entry.clear_epoch.load(Ordering::Acquire);
        if current != epoch {
            epoch = current;
            buffer.clear();
        }

        match msg.message_type {
            MessageType::Data | MessageType::DataEnd => {
                let is_end = msg.message_type == MessageType::DataEnd;
                buffer.extend_from_slice(&msg.payload);
                if !is_end {
                    continue;
                }
                let cmd = std::mem::take(&mut buffer);
                if !entry.has_access() {
                    refuse_locked(wr, "data").await?;
                    continue;
                }
                let expect_response = cmd.contains(&b'?');
                debug!(
                    "exec ({} bytes, expect_response={}): {}",
                    cmd.len(),
                    expect_response,
                    escape_bytes(&cmd)
                );
                let executed = match entry.device.execute(&cmd, expect_response).await {
                    Ok(r) => {
                        match &r.data {
                            Some(data) => {
                                debug!("resp ({} bytes): {}", data.len(), escape_bytes(data))
                            }
                            None => debug!("resp: (write-only, no read attempted)"),
                        }
                        r
                    }
                    Err(e) => {
                        warn!(
                            "device execute failed for cmd ({} bytes, expect_response={}) {:?}: {e:#}",
                            cmd.len(),
                            expect_response,
                            escape_bytes_truncated(&cmd, 120),
                        );
                        send_nonfatal(
                            wr,
                            NonFatalErrorCode::UnidentifiedError,
                            format!("device error: {e}"),
                        )
                        .await?;
                        continue;
                    }
                };

                // A device clear that landed while the bus transaction was in
                // flight abandons it: the reply is discarded and the client is
                // told which message it belonged to. We cannot abort the GPIB
                // read itself — it is one transaction, and half of one is
                // worse than all of it — so the abandonment is of the reply,
                // not of the bus traffic.
                let current = entry.clear_epoch.load(Ordering::Acquire);
                if current != epoch {
                    epoch = current;
                    debug!(
                        message_id = msg.message_parameter,
                        "device clear abandoned an in-flight message"
                    );
                    MessageType::Interrupted
                        .message_params(0, msg.message_parameter)
                        .no_payload()
                        .write_to(wr)
                        .await?;
                    wr.flush().await?;
                    continue;
                }

                if let Some(data) = executed.data {
                    write_response(wr, &entry, msg.message_parameter, &data).await?;
                }
                // Raised after the reply is on the wire: a client woken by the
                // service request finds the data already waiting for it.
                if let Some(stb) = executed.service_request {
                    entry.raise_service_request(stb);
                }
            }
            MessageType::Trigger => {
                if !entry.has_access() {
                    refuse_locked(wr, "trigger").await?;
                    continue;
                }
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

/// Smallest response chunk we will send, whatever the client declared. A
/// client whose maximum leaves no useful room after the 16-byte header is
/// misconfigured rather than genuinely constrained, and honouring it literally
/// would mean either an infinite stream of one-byte messages or a panic on a
/// zero-length chunk.
const MIN_CHUNK: u64 = 256;

/// Push a reply on the sync channel, split so that no message — header
/// included — exceeds the maximum the client declared with AsyncMaxMsgSize.
/// The client states its own limit in that request; the response states ours.
async fn write_response<W>(
    wr: &mut W,
    entry: &SessionEntry,
    message_id: u32,
    data: &[u8],
) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let client_max = *entry.client_max_message_size.lock().await;
    let max = client_max
        .saturating_sub(Message::MESSAGE_HEADER_SIZE as u64)
        .max(MIN_CHUNK) as usize;

    // An empty reply is still a reply: the client is waiting for a DataEND and
    // gets one, rather than blocking until its timeout.
    if data.is_empty() {
        MessageType::DataEnd
            .message_params(0, message_id)
            .no_payload()
            .write_to(wr)
            .await?;
        return wr.flush().await;
    }

    let mut chunks = data.chunks(max).peekable();
    while let Some(chunk) = chunks.next() {
        let ty = if chunks.peek().is_none() {
            MessageType::DataEnd
        } else {
            MessageType::Data
        };
        ty.message_params(0, message_id)
            .with_payload(chunk.to_vec())
            .write_to(wr)
            .await?;
    }
    wr.flush().await
}

/// Refuse an operation because another client holds the lock. HiSLIP has no
/// dedicated "locked" reply, so this is a non-fatal Error in the
/// device-defined code range: the session survives, the operation does not.
async fn refuse_locked<W>(wr: &mut W, what: &str) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    send_nonfatal(
        wr,
        NonFatalErrorCode::DeviceDefined(ERROR_LOCKED),
        format!("{what} refused: another client holds the lock"),
    )
    .await
}

/// Aborts a per-session forwarder task when the async channel goes away.
struct TaskGuard(tokio::task::JoinHandle<()>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Forward bus service requests to the client as `AsyncServiceRequest`.
///
/// On notification we serial-poll to learn the status byte, which the HiSLIP
/// message carries and which the poll also clears, so the device stops
/// asserting SRQ. Only a status with RQS set is forwarded: SRQ is a wired-OR
/// across the whole bus, so without that check another instrument pulling the
/// line would raise a spurious service request against this session's device.
///
/// The poll runs with no lock held. A serial poll is a bus transaction that can
/// take up to the GPIB timeout, and holding the write lock across it would stall
/// the async channel's replies for that whole time for no reason.
///
/// This is safe in either order, contrary to what an earlier revision of this
/// comment claimed: `Device::get_status` takes the bus mutex and drops it before
/// returning, so nothing is ever held across the two acquisitions and there is
/// no hold-and-wait cycle to deadlock on.
/// Poll this session's device for a service request, retrying while the SRQ
/// line stays asserted. `None` means this session has nothing to forward.
///
/// One poll per notification is not enough. A notification is an *edge*, but
/// SRQ is a wired-OR *level*: when a second device asserts while the first is
/// still holding the line low, there is no edge, so no notification, and its
/// request would never be seen. Observed on a two-instrument bus, where two
/// devices asked for service at almost the same moment and only the one that
/// happened to assert first was ever reported.
///
/// So while the line is still asserted, keep asking — either our device is
/// about to raise RQS, or somebody else's request is outstanding.
///
/// The retry is bounded. The line can legitimately stay low forever: a device
/// with no session bound to it can request service that nothing will ever
/// clear, and that must not turn into a busy loop.
async fn poll_for_service_request(entry: &SessionEntry) -> Option<u8> {
    let deadline = std::time::Instant::now() + SRQ_RECHECK_BUDGET;
    loop {
        match entry.device.get_status().await {
            Ok(stb) if stb & STB_RQS != 0 => return Some(stb),
            Ok(stb) => {
                // Not ours — at least not yet.
                match entry.device.srq_asserted().await {
                    Ok(true) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(SRQ_RECHECK_INTERVAL).await;
                    }
                    Ok(true) => {
                        debug!(
                            stb,
                            "srq still asserted after {:?}; another device is holding it \
                             and no session here can clear it",
                            SRQ_RECHECK_BUDGET
                        );
                        return None;
                    }
                    Ok(false) => {
                        debug!(stb, "srq released, nothing to forward");
                        return None;
                    }
                    // No level read available: fall back to the single poll
                    // this function used to do.
                    Err(e) => {
                        debug!(stb, "cannot read the srq line ({e:#}), not forwarding");
                        return None;
                    }
                }
            }
            Err(e) => {
                warn!("srq raised but serial poll failed, not forwarding: {e:#}");
                return None;
            }
        }
    }
}

async fn srq_forwarder<W>(
    mut srq: tokio::sync::broadcast::Receiver<()>,
    writer: Arc<Mutex<W>>,
    entry: Arc<SessionEntry>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match srq.recv().await {
            Ok(()) => {}
            // SRQ is a level, not a count: coalescing missed notifications
            // into one poll loses nothing.
            Err(RecvError::Lagged(n)) => debug!("srq notifications lagged by {n}"),
            Err(RecvError::Closed) => return,
        }

        let Some(stb) = poll_for_service_request(&entry).await else {
            continue;
        };
        // That poll took RQS from the instrument, so remember it for the
        // client's next status query.
        *entry.consumed_stb.lock().unwrap() = Some(stb);

        // Bound only the *acquisition*, and only to log: abandoning a wait for
        // a lock costs nothing, so a slow acquisition is reported and then
        // waited out rather than dropping the service request. The write below
        // is deliberately not bounded — cancelling it part-way would leave a
        // truncated HiSLIP message on the socket and desync the client for
        // good, which is far worse than blocking.
        let mut guard = loop {
            match tokio::time::timeout(WRITE_LOCK_STUCK_AFTER, writer.lock()).await {
                Ok(guard) => break guard,
                Err(_) => warn!(
                    "srq forwarder has waited {:?} for the write lock; \
                     the async channel may be stuck",
                    WRITE_LOCK_STUCK_AFTER
                ),
            }
        };

        debug!(stb, "forwarding service request");
        let write = async {
            MessageType::AsyncServiceRequest
                .message_params(stb, 0)
                .no_payload()
                .write_to(&mut *guard)
                .await?;
            (*guard).flush().await
        };
        if let Err(e) = write.await {
            debug!("srq forward failed, client likely gone: {e}");
            return;
        }
    }
}

/// Push the service requests the server raises on its own behalf.
///
/// These have no SRQ line behind them, so there is nothing to serial-poll and
/// nothing to clear: the status byte is decided by whoever queued it. Today
/// that is the sync loop announcing MAV for a reply it has just delivered,
/// which the instrument cannot announce itself because the read that produced
/// the reply is what cleared its MAV bit.
async fn self_raised_srq_forwarder<W>(mut rx: mpsc::Receiver<u8>, writer: Arc<Mutex<W>>)
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    while let Some(stb) = rx.recv().await {
        let mut guard = writer.lock().await;
        debug!(stb, "raising service request on the server's own behalf");
        let write = async {
            MessageType::AsyncServiceRequest
                .message_params(stb, 0)
                .no_payload()
                .write_to(&mut *guard)
                .await?;
            (*guard).flush().await
        };
        if let Err(e) = write.await {
            debug!("service request push failed, client likely gone: {e}");
            return;
        }
    }
}

async fn async_loop<R, W>(
    rd: &mut R,
    wr: W,
    entry: Arc<SessionEntry>,
    config: Config,
) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let writer = Arc::new(Mutex::new(wr));

    // Push service requests from a separate task: the loop below spends most
    // of its life parked in a read, and `Message::read_from` is not
    // cancel-safe, so it cannot be raced against the SRQ channel in a select.
    let _srq_task = match entry.device.subscribe_srq().await {
        Some(rx) => Some(TaskGuard(tokio::spawn(srq_forwarder(
            rx,
            writer.clone(),
            entry.clone(),
        )))),
        None => {
            debug!("adapter cannot report SRQ; bus service requests will not be pushed");
            None
        }
    };

    // The other source: service requests the server raises itself. Separate
    // task for the same reason — the loop below is parked in a read almost all
    // of the time.
    let _self_srq_task = entry
        .srq_rx
        .lock()
        .await
        .take()
        .map(|rx| TaskGuard(tokio::spawn(self_raised_srq_forwarder(rx, writer.clone()))));

    loop {
        // Read with the writer unlocked, so the forwarder can push meanwhile.
        let incoming = Message::read_from(rd, config.max_message_size).await?;

        // A contended lock request waits for the holder to let go, which can be
        // the client's whole timeout. Do that before taking the write lock:
        // parking this client is what it asked for, parking the service-request
        // forwarders behind it is not.
        let mut lock_answer = None;
        if let Ok(msg) = &incoming {
            if msg.message_type == MessageType::AsyncLock && msg.control_code != 0 {
                let lock_string = String::from_utf8_lossy(&msg.payload).into_owned();
                let timeout = Duration::from_millis(msg.message_parameter as u64);
                let answer = entry
                    .locks
                    .request(&entry.resource, entry.id, &lock_string, timeout)
                    .await;
                debug!(
                    session = entry.id,
                    %lock_string,
                    ?timeout,
                    ?answer,
                    "lock request"
                );
                lock_answer = Some(answer);
            }
        }

        let mut guard = writer.lock().await;
        let wr = &mut *guard;

        let msg = match incoming {
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
                // Control code 0 releases, 1 requests. A request was answered
                // above, before the write lock; a release never waits, so it
                // can happen here.
                let answer = match lock_answer {
                    Some(answer) => answer,
                    None => {
                        let answer = entry.locks.release(&entry.resource, entry.id);
                        debug!(session = entry.id, ?answer, "lock release");
                        answer
                    }
                };
                MessageType::AsyncLockResponse
                    .message_params(answer.control_code(), 0)
                    .no_payload()
                    .write_to(wr)
                    .await?;
                wr.flush().await?;
            }
            MessageType::AsyncLockInfo => {
                let (exclusive, holders) = entry.locks.info(&entry.resource);
                MessageType::AsyncLockInfoResponse
                    .message_params(u8::from(exclusive), holders)
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
                if !entry.has_access() {
                    refuse_locked(wr, "device clear").await?;
                    continue;
                }
                // Bump before touching the bus. The sync loop compares this
                // across its operation, so it has to change while that
                // operation is still running for the reply to be abandoned.
                entry.clear_epoch.fetch_add(1, Ordering::Release);
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
                // HiSLIP has no error reply for a status query — the response
                // carries a status byte and nothing else — so a failed poll can
                // only be reported as 0. Log it, so it is at least visible as a
                // failure rather than passing for a genuine "nothing to report".
                let mut stb = match entry.device.get_status().await {
                    Ok(stb) => stb,
                    Err(e) => {
                        warn!("hislip status query failed, reporting 0: {e:#}");
                        0
                    }
                };
                // If the instrument has nothing to report, it may be because
                // the daemon already took it: a serial poll clears RQS, and
                // the daemon has to poll to fill in an AsyncServiceRequest.
                // Hand that byte over rather than let the client read a bit
                // that was consumed on its behalf. Only when the live poll is
                // silent, so a real reading is never overwritten.
                if stb == 0 {
                    if let Some(consumed) = entry.take_consumed_stb() {
                        debug!(consumed, "reporting the status byte we polled earlier");
                        stb = consumed;
                    }
                } else {
                    entry.take_consumed_stb();
                }
                MessageType::AsyncStatusResponse
                    .message_params(stb, 0)
                    .no_payload()
                    .write_to(wr)
                    .await?;
                wr.flush().await?;
            }
            MessageType::AsyncStartTLS | MessageType::AsyncEndTLS
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

/// Parse a HiSLIP subaddress into an optional GPIB primary address.
///
/// Supported forms:
/// - `hislip<N>` / `gpib<N>` / `inst<N>` — trailing digits are the PAD
/// - `<N>` — bare numeric subaddress
/// - `foo,N` / `foo:N` — explicit separator form (PAD is after the last
///   separator). Note: most VISA clients collapse `foo,N` into
///   `sub_address=foo, port=N` before the string ever reaches us, so
///   clients typically want the no-separator form.
///
/// The literal `hislip0` and `gpib0` stay reserved for "use the
/// daemon-configured default PAD" so existing scripts that treat
/// `TCPIP::host::hislip0::INSTR` as a generic handle still work.
pub fn parse_subaddress_pad(sub: &str) -> Option<u8> {
    let s = sub.trim().trim_end_matches('\0');
    if s.is_empty() || s.eq_ignore_ascii_case("hislip0") || s.eq_ignore_ascii_case("gpib0") {
        return None;
    }
    let tail = s.rsplit([',', ':']).next().unwrap_or(s);
    let digits: String = tail
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if digits.is_empty() {
        return None;
    }
    if let Ok(n) = digits.parse::<u8>() {
        if n <= 30 {
            return Some(n);
        }
    }
    None
}

/// Render bytes as a single-line string with non-printable bytes escaped as
/// `\xNN`. Common control characters get C-style escapes (`\n \r \t \\`) so
/// SCPI traffic stays readable in the log. Used by the `-v` HiSLIP cmd/response
/// dump.
fn escape_bytes(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            b'\t' => s.push_str("\\t"),
            b'\\' => s.push_str("\\\\"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}

fn escape_bytes_truncated(bytes: &[u8], max: usize) -> String {
    if bytes.len() <= max {
        escape_bytes(bytes)
    } else {
        let mut s = escape_bytes(&bytes[..max]);
        s.push_str(&format!("… (+{} bytes)", bytes.len() - max));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pad() {
        // pyvisa-compatible no-separator forms
        assert_eq!(parse_subaddress_pad("hislip14"), Some(14));
        assert_eq!(parse_subaddress_pad("hislip16"), Some(16));
        assert_eq!(parse_subaddress_pad("gpib7"), Some(7));
        assert_eq!(parse_subaddress_pad("inst3"), Some(3));
        // bare numeric
        assert_eq!(parse_subaddress_pad("14"), Some(14));
        // explicit-separator forms (for clients that don't strip commas)
        assert_eq!(parse_subaddress_pad("gpib0,14"), Some(14));
        assert_eq!(parse_subaddress_pad("hislip0,7"), Some(7));
        // reserved defaults
        assert_eq!(parse_subaddress_pad("hislip0"), None);
        assert_eq!(parse_subaddress_pad("gpib0"), None);
        assert_eq!(parse_subaddress_pad(""), None);
        // out of range
        assert_eq!(parse_subaddress_pad("gpib0,31"), None);
        assert_eq!(parse_subaddress_pad("hislip31"), None);
    }

    #[test]
    fn standard_port_is_4880() {
        assert_eq!(super::super::STANDARD_PORT, 4880);
    }

    #[test]
    fn escape_keeps_printable_and_escapes_controls() {
        assert_eq!(escape_bytes(b"*IDN?"), "*IDN?");
        assert_eq!(escape_bytes(b"hi\nthere\r\n"), "hi\\nthere\\r\\n");
        assert_eq!(escape_bytes(b"tab\there"), "tab\\there");
        assert_eq!(escape_bytes(b"back\\slash"), "back\\\\slash");
        // 0x01 is non-printable, no shortcut → \x01
        assert_eq!(escape_bytes(&[0x01, b'A', 0xff]), "\\x01A\\xff");
        // 0x7e (~) is the last printable, 0x7f (DEL) is not
        assert_eq!(escape_bytes(&[0x7e, 0x7f]), "~\\x7f");
    }

    #[test]
    fn escape_truncated_appends_suffix() {
        assert_eq!(escape_bytes_truncated(b"abcdef", 10), "abcdef");
        assert_eq!(escape_bytes_truncated(b"abcdef", 6), "abcdef");
        assert_eq!(escape_bytes_truncated(b"abcdefghij", 4), "abcd… (+6 bytes)");
    }
}
