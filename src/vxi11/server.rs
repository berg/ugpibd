// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// The VXI-11 core channel (spec §B.6): one TCP listener, ONC-RPC records in,
// device operations out.
//
// This is the front-end where a client's read is an explicit wire message
// (`device_read`), so the daemon addresses the instrument to talk because it
// was asked to — the HiSLIP read-after-write heuristic has no counterpart
// here and never will. Each RPC carries its own io_timeout, which is applied
// to the bus for that operation and the daemon default restored after, so a
// slow screen dump and a fast query stop sharing one global number.
//
// Locking rides the daemon-wide registry (exclusive, per-link, non-nesting
// — RULE B.6.72 — and coherent with HiSLIP viLocks on the same
// instrument); the abort channel is real (DEVICE_ASYNC on its own port,
// terminating in-flight operations at their nearest safe point); and the
// interrupt channel calls the client back over TCP with the registered
// handle when an enabled link's instrument pulls SRQ. Not yet here,
// refused honestly rather than stubbed: device_docmd (error 8), the
// VXI-11.2 interface-device phase of docs/VXI11-PLAN.md.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::BufStream;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::frontend::lock::{self, LockRegistry};

use super::messages::*;
use super::rpc;
use super::xdr::{self, XdrError};
use super::{ErrorCode, *};
use crate::frontend::instrument::{Instrument, MAX_READ};

/// Fallback port. VXI-11 has no IANA fixed port — the spec expects portmap
/// discovery — but pyvisa-py carries the port in the resource string
/// (`TCPIP::host,9010::gpib0,18::INSTR`), which is the supported way in.
pub const DEFAULT_PORT: u16 = 9010;

#[derive(Debug, Clone)]
pub struct Config {
    /// Largest device_write data accepted, reported in create_link
    /// (RULE B.6.3 requires ≥ 1024).
    pub max_recv_size: u32,
    /// Active-link cap; create_link past it answers 9, out of resources
    /// (RULE B.6.5).
    pub max_links: usize,
    /// The daemon's bus timeout: applied when an RPC passes io_timeout 0,
    /// and restored after every operation that overrode it.
    pub default_io_timeout_ms: u32,
    /// The PAD `inst0` and bare `gpib0` resolve to — the daemon's
    /// `--default-address`, same convention as the HiSLIP sub-addresses.
    pub default_pad: u8,
    /// The lock registry this server enforces — the same one every other
    /// front-end consults, passed in by the daemon (see hislip's Config for
    /// the reasoning). VXI-11 locks are exclusive, per-link, non-nesting
    /// (RULE B.6.72); the identities are namespaced so a HiSLIP session and
    /// a VXI-11 link can never collide.
    pub locks: Arc<LockRegistry>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_recv_size: 65536,
            max_links: 64,
            default_io_timeout_ms: 3000,
            default_pad: 0,
            locks: Arc::new(LockRegistry::new()),
        }
    }
}

/// What a create_link device name resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAddr {
    /// A device at this primary address.
    Pad(u8),
    /// The IEEE 488.1 interface itself (VXI-11.2 RULE B.1.3): the link for
    /// device_docmd and the bus-wide control sequences.
    Interface,
}

/// Parse a create_link device name.
///
/// Accepted spellings (VXI-11.2 §B.1): `gpib0,<pad>` addresses a device on
/// the one bus this daemon controls; a bare `gpib0` is the *interface*
/// (RULE B.1.3); `inst0` means the daemon's default PAD, matching the
/// HiSLIP sub-address convention. Errors are create_link's own table
/// (B.4): a secondary address parses but is refused with 21 until the
/// backend can address one — not silently dropped; an interface number
/// other than 0 is 3, device not accessible (the daemon has one bus);
/// anything unparseable is 1, syntax error.
pub fn parse_device_name(name: &str, default_pad: u8) -> Result<DeviceAddr, ErrorCode> {
    let name = name.trim().to_ascii_lowercase();
    if name == "inst0" {
        return Ok(DeviceAddr::Pad(default_pad));
    }
    if name == "gpib0" {
        return Ok(DeviceAddr::Interface);
    }
    let Some(rest) = name.strip_prefix("gpib") else {
        return Err(ErrorCode::SyntaxError);
    };
    let mut parts = rest.split(',');
    // A malformed interface number is a syntax error; a well-formed one
    // that names a bus this daemon does not have (it has exactly one) is
    // "device not accessible" — the name is legal, the hardware absent.
    let Ok(interface) = parts.next().unwrap_or("").parse::<u8>() else {
        return Err(ErrorCode::SyntaxError);
    };
    if interface != 0 {
        return Err(ErrorCode::DeviceNotAccessible);
    }
    let Some(pad_str) = parts.next() else {
        return Err(ErrorCode::SyntaxError);
    };
    let Ok(pad) = pad_str.parse::<u8>() else {
        return Err(ErrorCode::SyntaxError);
    };
    if pad > 30 {
        return Err(ErrorCode::InvalidAddress);
    }
    match parts.next() {
        // A secondary address is real VXI-11.2, and the backends cannot
        // address one yet. 21 is a loud refusal the client sees; swallowing
        // the sad and talking to the primary would answer the wrong device.
        Some(_) => Err(ErrorCode::InvalidAddress),
        None => Ok(DeviceAddr::Pad(pad)),
    }
}

struct Link {
    lid: i32,
    /// Interface links (bare `gpib0`) use `instrument` purely as a bus
    /// handle; its PAD is never addressed by their operations.
    kind: DeviceAddr,
    instrument: Arc<Instrument>,
    /// An abortable operation is running on this link right now. device_abort
    /// only terminates an *in-progress* call (OBSERVATION B.6.24); one that
    /// lands between calls is a no-op, not a poison pill for the next.
    in_flight: AtomicBool,
    /// Set by device_abort while `in_flight`; consumed (or discarded) when
    /// the operation it targeted finishes.
    aborted: AtomicBool,
    /// Wakes waits that hold no bus transaction (the lock wait). Bus reads
    /// notice `aborted` between poll slices instead — a slice, once started,
    /// is never torn down mid-flight.
    abort_notify: Notify,
    /// `Some(handle)` while this link wants device_intr_srq calls; the bytes
    /// go back exactly as registered (RULE B.6.111). Per-link state,
    /// deliberately independent of the interrupt channel's existence
    /// (OBSERVATION B.6.21).
    srq_handle: Mutex<Option<Vec<u8>>>,
}

impl Link {
    fn holder(&self) -> lock::HolderId {
        lock::vxi11_id(self.lid)
    }

    fn resource(&self) -> String {
        match self.kind {
            // Locks on the interface link are scoped to the interface, not
            // to whatever PAD its bus handle happens to carry.
            DeviceAddr::Interface => "gpib0:intf".to_string(),
            DeviceAddr::Pad(_) => self.instrument.resource_key(),
        }
    }

    fn is_interface(&self) -> bool {
        matches!(self.kind, DeviceAddr::Interface)
    }
}

/// Marks the link's abortable operation window. Dropping it closes the
/// window and discards any abort that was not consumed — the call it
/// targeted is over, and RULE B.6.24 does not let it outlive that call.
struct OpWindow<'a>(&'a Link);

impl<'a> OpWindow<'a> {
    fn open(link: &'a Link) -> Self {
        link.in_flight.store(true, Ordering::Release);
        Self(link)
    }

    /// Did an abort land during this window? Consuming it here (rather than
    /// on drop) lets the operation shape its reply — RULE B.6.21/B.6.30 want
    /// the transferred byte count and partial data reported even on abort.
    fn take_abort(&self) -> bool {
        self.0.aborted.swap(false, Ordering::AcqRel)
    }
}

impl Drop for OpWindow<'_> {
    fn drop(&mut self) {
        self.0.aborted.store(false, Ordering::Release);
        self.0.in_flight.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct LinkTable {
    links: HashMap<i32, Arc<Link>>,
}

/// Server-wide state: the link table is shared across connections because
/// the abort channel (a different TCP connection) names links by id.
struct Shared {
    config: Config,
    links: Mutex<LinkTable>,
    next_lid: AtomicI32,
    /// Where the abort listener answers, reported in every Create_LinkResp.
    abort_port: u16,
}

/// Server entry point. `instrument_for` maps a parsed PAD to the instrument
/// handle serving it. The abort channel (DEVICE_ASYNC) is bound alongside on
/// an ephemeral port of the same address; clients learn it from create_link.
pub async fn run<F>(listener: TcpListener, config: Config, instrument_for: F) -> io::Result<()>
where
    F: Fn(u8) -> Arc<Instrument> + Send + Sync + 'static,
{
    let core_addr = listener.local_addr()?;
    let abort_listener = TcpListener::bind((core_addr.ip(), 0)).await?;
    let abort_port = abort_listener.local_addr()?.port();
    info!("VXI-11 core listening on {core_addr}, abort channel on port {abort_port}");
    let shared = Arc::new(Shared {
        config,
        links: Mutex::new(LinkTable::default()),
        next_lid: AtomicI32::new(1),
        abort_port,
    });
    let abort_shared = shared.clone();
    tokio::spawn(async move {
        if let Err(e) = run_abort_channel(abort_listener, abort_shared).await {
            warn!("vxi11 abort channel died: {e:#}");
        }
    });
    let instrument_for = Arc::new(instrument_for);
    loop {
        let (stream, addr) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let shared = shared.clone();
        let instrument_for = instrument_for.clone();
        tokio::spawn(async move {
            debug!("vxi11 client connected addr={addr}");
            match serve_connection(stream, shared, instrument_for).await {
                Ok(()) => debug!("vxi11 client disconnected addr={addr}"),
                Err(e) => warn!("vxi11 client error: {e:#} addr={addr}"),
            }
        });
    }
}

/// The abort channel: DEVICE_ASYNC, one procedure. Kept apart from the core
/// dispatch on purpose — it must answer while a core call is blocked on the
/// bus, which is the whole reason it exists (§B.6.16).
async fn run_abort_channel(listener: TcpListener, shared: Arc<Shared>) -> io::Result<()> {
    loop {
        let (stream, addr) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let shared = shared.clone();
        tokio::spawn(async move {
            let mut stream = BufStream::new(stream);
            loop {
                let record = match rpc::read_record(&mut stream, 4096).await {
                    Ok(Some(r)) => r,
                    Ok(None) => break,
                    Err(e) => {
                        debug!("vxi11 abort connection error: {e} addr={addr}");
                        break;
                    }
                };
                let reply = match rpc::decode_call(&record) {
                    Ok((header, args)) => abort_dispatch(&shared, header, args),
                    Err(rpc::CallError::RpcVersion { xid, .. }) => rpc::reply_rpc_mismatch(xid),
                    Err(e) => {
                        debug!("vxi11 abort channel: {e}");
                        break;
                    }
                };
                if rpc::write_record(&mut stream, &reply).await.is_err() {
                    break;
                }
            }
        });
    }
}

fn abort_dispatch(shared: &Shared, header: rpc::CallHeader, args: &[u8]) -> Vec<u8> {
    let xid = header.xid;
    if header.prog != DEVICE_ASYNC_PROG {
        return rpc::reply_prog_unavail(xid);
    }
    if header.vers != DEVICE_ASYNC_VERS {
        return rpc::reply_prog_mismatch(xid, DEVICE_ASYNC_VERS, DEVICE_ASYNC_VERS);
    }
    if header.proc != DEVICE_ABORT {
        return rpc::reply_proc_unavail(xid);
    }
    let Ok(lid) = xdr::Cursor::new(args).i32() else {
        return rpc::reply_garbage_args(xid);
    };
    let error = match link_for(shared, lid) {
        // RULE B.6.106/OBSERVATION B.6.24: terminate the in-progress call if
        // there is one; success either way means "delivered", nothing more
        // (OBSERVATION B.6.25). RULE B.6.109: no lock check here.
        Some(link) => {
            if link.in_flight.load(Ordering::Acquire) {
                link.aborted.store(true, Ordering::Release);
                link.abort_notify.notify_one();
                debug!("vxi11 abort delivered to link {lid}");
            }
            ErrorCode::NoError
        }
        None => ErrorCode::InvalidLinkIdentifier,
    };
    rpc::reply_success(xid, &encode_device_error(error.as_u32()))
}

/// Per-core-connection state beyond the socket itself.
struct ConnState {
    /// Links created on this connection, destroyed with it: the channel
    /// closing is how a crashed client's links are recovered (§B.2).
    /// Shared with the SRQ forwarder, which needs to know which links are
    /// this connection's when fanning a service request out.
    owned: Arc<Mutex<Vec<i32>>>,
    /// The interrupt channel, at most one per connection (RULE B.6.89).
    intr: Option<IntrChannel>,
    /// The address the client called from; the interrupt channel connects
    /// back to the address the client *names*, but a client naming a
    /// different host than it called from is worth a log line before this
    /// daemon opens a TCP connection to it.
    peer: std::net::IpAddr,
}

/// A live interrupt channel: a sender task owning the TCP connection to the
/// client's DEVICE_INTR server, fed handles through `tx`, plus the SRQ
/// forwarder watching the bus. Both die with the channel — but the
/// forwarder is asked, never aborted: `JoinHandle::abort` cancels at any
/// await point, and the forwarder's serial polls hold the bus. A poll
/// killed between Serial Poll Enable and its closing SPD/UNT strands the
/// whole bus in serial-poll state, where every later write times out —
/// observed on hardware, from exactly this drop. The shutdown permit is
/// consumed only at the loop top, between bus operations.
struct IntrChannel {
    shutdown: Arc<Notify>,
    sender: tokio::task::JoinHandle<()>,
    /// For notifications outside the forwarder's SRQ loop — RULE B.4.14's
    /// enable-while-the-line-is-already-high case.
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

impl Drop for IntrChannel {
    fn drop(&mut self) {
        // The forwarder exits at its next loop top; its tx drops with it,
        // which ends the sender's channel. The abort is belt-and-braces for
        // a sender blocked in socket I/O, which holds no bus state.
        self.shutdown.notify_one();
        self.sender.abort();
    }
}

async fn serve_connection<F>(
    stream: tokio::net::TcpStream,
    shared: Arc<Shared>,
    instrument_for: Arc<F>,
) -> anyhow::Result<()>
where
    F: Fn(u8) -> Arc<Instrument> + Send + Sync + 'static,
{
    let peer = stream.peer_addr()?.ip();
    let mut stream = BufStream::new(stream);
    let mut conn = ConnState {
        owned: Arc::new(Mutex::new(Vec::new())),
        intr: None,
        peer,
    };
    let connection_result = loop {
        // The record cap is deliberately above maxRecvSize plus envelope:
        // RULE B.6.16 answers an over-limit device_write with error 5, which
        // requires *reading* the offending record first. Twice the limit is
        // the courtesy ceiling; a record beyond that is not a confused
        // client, and the connection closes on it.
        let max_record = shared.config.max_recv_size as usize * 2 + 1024;
        let record = match rpc::read_record(&mut stream, max_record).await? {
            Some(r) => r,
            None => break Ok(()),
        };
        let reply = match rpc::decode_call(&record) {
            Ok((header, args)) => {
                dispatch(&shared, instrument_for.as_ref(), &mut conn, header, args)
                    .await
                    .unwrap_or_else(rpc::reply_garbage_args)
            }
            Err(rpc::CallError::RpcVersion { xid, .. }) => rpc::reply_rpc_mismatch(xid),
            Err(e @ rpc::CallError::NotACall { .. }) => {
                // Nothing to reply to; record framing is intact, so the
                // connection can continue.
                warn!("vxi11: {e}");
                continue;
            }
            Err(e @ rpc::CallError::Garbled(_)) => {
                warn!("vxi11: {e}");
                continue;
            }
        };
        rpc::write_record(&mut stream, &reply).await?;
    };
    // RULE B.6.77: locks are tied to the core connection — a broken
    // connection releases everything its links held, then the links go too,
    // and the interrupt channel with them (dropped with `conn`).
    let removed: Vec<Arc<Link>> = {
        let owned = conn.owned.lock().unwrap();
        let mut links = shared.links.lock().unwrap();
        owned
            .iter()
            .filter_map(|lid| links.links.remove(lid))
            .collect()
    };
    for link in removed {
        shared.config.locks.release_all(link.holder());
    }
    connection_result
}

/// Dispatch one call. `Err(xid)` means the arguments did not decode —
/// GARBAGE_ARGS per RFC 5531, since the procedure was reachable but its
/// parameters were not.
async fn dispatch<F>(
    shared: &Arc<Shared>,
    instrument_for: &F,
    conn: &mut ConnState,
    header: rpc::CallHeader,
    args: &[u8],
) -> Result<Vec<u8>, u32>
where
    F: Fn(u8) -> Arc<Instrument>,
{
    let xid = header.xid;
    if header.prog != DEVICE_CORE_PROG {
        return Ok(rpc::reply_prog_unavail(xid));
    }
    if header.vers != DEVICE_CORE_VERS {
        return Ok(rpc::reply_prog_mismatch(
            xid,
            DEVICE_CORE_VERS,
            DEVICE_CORE_VERS,
        ));
    }
    let garbage = |_: XdrError| xid;
    let results = match header.proc {
        CREATE_LINK => {
            let parms = CreateLinkParms::decode(args).map_err(garbage)?;
            create_link(shared, instrument_for, conn, parms)
                .await
                .encode()
        }
        DEVICE_WRITE => {
            match DeviceWriteParms::decode(args, shared.config.max_recv_size) {
                Ok(parms) => device_write(shared, parms).await.encode(),
                // RULE B.6.16: data over the create_link-promised
                // maxRecvSize is a parameter error, nothing transferred.
                Err(XdrError::TooLong { .. }) => DeviceWriteResp {
                    error: ErrorCode::ParameterError.as_u32(),
                    size: 0,
                }
                .encode(),
                Err(e) => return Err(garbage(e)),
            }
        }
        DEVICE_READ => {
            let parms = DeviceReadParms::decode(args).map_err(garbage)?;
            device_read(shared, parms).await.encode()
        }
        DEVICE_READSTB => {
            let parms = DeviceGenericParms::decode(args).map_err(garbage)?;
            device_readstb(shared, parms).await.encode()
        }
        DEVICE_TRIGGER | DEVICE_CLEAR | DEVICE_REMOTE | DEVICE_LOCAL => {
            let parms = DeviceGenericParms::decode(args).map_err(garbage)?;
            let error = device_simple(shared, header.proc, parms).await;
            encode_device_error(error)
        }
        DESTROY_LINK => {
            let lid = xdr::Cursor::new(args).i32().map_err(garbage)?;
            let removed = {
                let mut links = shared.links.lock().unwrap();
                links.links.remove(&lid)
            };
            // RULE B.6.10: an unknown lid is 4. RULE B.6.11: destroying a
            // link touches no device state, so there is no bus traffic here.
            // RULE B.6.9.2: its lock goes with it. RULE B.6.12: abort does
            // not touch this (no OpWindow).
            let error = if let Some(link) = removed {
                conn.owned.lock().unwrap().retain(|&l| l != lid);
                shared.config.locks.release_all(link.holder());
                ErrorCode::NoError
            } else {
                ErrorCode::InvalidLinkIdentifier
            };
            encode_device_error(error.as_u32())
        }
        DEVICE_LOCK => {
            let parms = DeviceLockParms::decode(args).map_err(garbage)?;
            encode_device_error(device_lock(shared, parms).await.as_u32())
        }
        DEVICE_UNLOCK => {
            // RULE B.6.81: not abortable (no OpWindow).
            let lid = xdr::Cursor::new(args).i32().map_err(garbage)?;
            let error = match link_for(shared, lid) {
                Some(link) => {
                    let locks = &shared.config.locks;
                    if locks.holds(&link.resource(), link.holder()) {
                        locks.release(&link.resource(), link.holder());
                        ErrorCode::NoError
                    } else {
                        // RULE B.6.80.
                        ErrorCode::NoLockHeldByThisLink
                    }
                }
                None => ErrorCode::InvalidLinkIdentifier,
            };
            encode_device_error(error.as_u32())
        }
        DEVICE_ENABLE_SRQ => {
            // RULE B.6.95: no lock gate. RULE B.6.106: not abortable.
            let parms = DeviceEnableSrqParms::decode(args).map_err(garbage)?;
            let error = match link_for(shared, parms.lid) {
                Some(link) => {
                    // OBSERVATION B.6.21: this state belongs to the link and
                    // ignores whether an interrupt channel currently exists.
                    let was_enabled = {
                        let mut handle = link.srq_handle.lock().unwrap();
                        let was = handle.is_some();
                        *handle = parms.enable.then(|| parms.handle.clone());
                        was
                    };
                    // RULE B.4.14 (VXI-11.2): enabling while the SRQ line is
                    // already high delivers a notification immediately — the
                    // edge the forwarder waits for already happened.
                    if parms.enable && !was_enabled {
                        if let Some(intr) = &conn.intr {
                            let high = link.instrument.srq_asserted().await.unwrap_or(false);
                            if high {
                                let _ = intr.tx.send(parms.handle);
                            }
                        }
                    }
                    ErrorCode::NoError
                }
                // RULE B.6.94.
                None => ErrorCode::InvalidLinkIdentifier,
            };
            encode_device_error(error.as_u32())
        }
        CREATE_INTR_CHAN => {
            let parms = DeviceRemoteFunc::decode(args).map_err(garbage)?;
            encode_device_error(
                create_intr_chan(shared, instrument_for, conn, parms)
                    .await
                    .as_u32(),
            )
        }
        DESTROY_INTR_CHAN => {
            // RULE B.6.91/B.6.92: close it, or report there is none (6).
            let error = match conn.intr.take() {
                Some(channel) => {
                    drop(channel);
                    ErrorCode::NoError
                }
                None => ErrorCode::ChannelNotEstablished,
            };
            encode_device_error(error.as_u32())
        }
        DEVICE_DOCMD => {
            let parms = DeviceDocmdParms::decode(args).map_err(garbage)?;
            device_docmd(shared, parms).await.encode()
        }
        _ => return Ok(rpc::reply_proc_unavail(xid)),
    };
    Ok(rpc::reply_success(xid, &results))
}

async fn create_link<F>(
    shared: &Shared,
    instrument_for: &F,
    conn: &ConnState,
    parms: CreateLinkParms,
) -> CreateLinkResp
where
    F: Fn(u8) -> Arc<Instrument>,
{
    let refuse = |error: ErrorCode| CreateLinkResp {
        error: error.as_u32(),
        lid: 0,
        abort_port: 0,
        max_recv_size: 0,
    };
    let name = String::from_utf8_lossy(&parms.device).into_owned();
    let kind = match parse_device_name(&name, shared.config.default_pad) {
        Ok(kind) => kind,
        Err(e) => {
            debug!("create_link refused for device name {name:?}: {e:?}");
            return refuse(e);
        }
    };
    let instrument = instrument_for(match kind {
        DeviceAddr::Pad(pad) => pad,
        // Bus handle only; interface operations never address this PAD.
        DeviceAddr::Interface => shared.config.default_pad,
    });
    let lid = {
        let mut links = shared.links.lock().unwrap();
        if links.links.len() >= shared.config.max_links {
            return refuse(ErrorCode::OutOfResources);
        }
        let lid = shared.next_lid.fetch_add(1, Ordering::Relaxed);
        links.links.insert(
            lid,
            Arc::new(Link {
                lid,
                kind,
                instrument,
                in_flight: AtomicBool::new(false),
                aborted: AtomicBool::new(false),
                abort_notify: Notify::new(),
                srq_handle: Mutex::new(None),
            }),
        );
        conn.owned.lock().unwrap().push(lid);
        lid
    };
    // RULES B.6.3.1 and B.6.7: with lockDevice set, the lock is acquired —
    // waiting up to lock_timeout — or no link comes into being. The link is
    // inserted first because the lock needs its identity, and unwound on
    // failure; the window where it exists unlocked is invisible to the
    // client, which has no lid yet. OBSERVATION B.6.2: not abortable, there
    // is nothing to name in a device_abort.
    if parms.lock_device {
        let link = link_for(shared, lid).expect("just inserted");
        let granted = shared
            .config
            .locks
            .request(
                &link.resource(),
                link.holder(),
                "",
                Duration::from_millis(u64::from(parms.lock_timeout_ms)),
            )
            .await;
        if !granted.is_success() {
            let mut links = shared.links.lock().unwrap();
            links.links.remove(&lid);
            conn.owned.lock().unwrap().retain(|&l| l != lid);
            return refuse(ErrorCode::DeviceLockedByAnotherLink);
        }
    }
    debug!("vxi11 link {lid} created for {name:?}");
    CreateLinkResp {
        error: ErrorCode::NoError.as_u32(),
        lid,
        abort_port: shared.abort_port,
        max_recv_size: shared.config.max_recv_size,
    }
}

async fn device_lock(shared: &Shared, parms: DeviceLockParms) -> ErrorCode {
    // RULE B.6.73: unknown lid answers 4 before any lock work.
    let Some(link) = link_for(shared, parms.lid) else {
        return ErrorCode::InvalidLinkIdentifier;
    };
    let locks = &shared.config.locks;
    let (resource, holder) = (link.resource(), link.holder());
    // RULE B.6.72: VXI-11 locks do not nest — a re-lock by the holder is
    // the same error a stranger gets.
    if locks.holds(&resource, holder) {
        return ErrorCode::DeviceLockedByAnotherLink;
    }
    // RULE B.6.74: without waitlock, one immediate attempt.
    let timeout = if parms.flags & OP_FLAG_WAITLOCK != 0 {
        Duration::from_millis(u64::from(parms.lock_timeout_ms))
    } else {
        Duration::ZERO
    };
    // RULE B.6.76: the wait is abortable. This wait holds no bus
    // transaction, so it can be interrupted at any moment — the notify path
    // rather than the between-slices flag check the bus operations use.
    let window = OpWindow::open(&link);
    let request = locks.request(&resource, holder, "", timeout);
    tokio::pin!(request);
    let granted = loop {
        tokio::select! {
            granted = &mut request => break granted,
            _ = link.abort_notify.notified() => {
                if window.take_abort() {
                    return ErrorCode::Abort;
                }
                // A stale wakeup (permit left by an abort a previous window
                // already discarded): keep waiting.
            }
        }
    };
    if granted.is_success() {
        ErrorCode::NoError
    } else {
        // RULE B.6.75.
        ErrorCode::DeviceLockedByAnotherLink
    }
}

/// RQS in a serial-poll status byte: the device is the one requesting
/// service on the wired-OR SRQ line.
const STB_RQS: u8 = 0x40;

/// IEEE 488.1 universal commands used by the interface link.
const GPIB_DCL: u8 = 0x14;
const GPIB_GET: u8 = 0x08;

async fn create_intr_chan<F>(
    shared: &Arc<Shared>,
    instrument_for: &F,
    conn: &mut ConnState,
    parms: DeviceRemoteFunc,
) -> ErrorCode
where
    F: Fn(u8) -> Arc<Instrument>,
{
    // RULES B.6.85-B.6.88, all error 8: unknown family, UDP (permitted to
    // be unsupported, and it is — SRQ delivery that can vanish or reorder
    // is not worth its latency win here), wrong program, wrong version.
    if parms.prog_family != DEVICE_TCP {
        return ErrorCode::OperationNotSupported;
    }
    if parms.prog_num != DEVICE_INTR_PROG || parms.prog_vers != DEVICE_INTR_VERS {
        return ErrorCode::OperationNotSupported;
    }
    // RULE B.6.89: one channel per connection.
    if conn.intr.is_some() {
        return ErrorCode::ChannelAlreadyEstablished;
    }
    let host = std::net::Ipv4Addr::from(parms.host_addr);
    if std::net::IpAddr::from(host) != conn.peer {
        // Legal (the spec only observes that clients "normally" name
        // themselves) but unusual enough to leave a trace: this daemon is
        // about to open a TCP connection to a third party on request.
        info!(
            "vxi11 interrupt channel requested to {host}:{} by client at {}",
            parms.host_port, conn.peer
        );
    }
    let stream = match tokio::net::TcpStream::connect((host, parms.host_port)).await {
        Ok(s) => s,
        Err(e) => {
            // RULE B.6.83.
            debug!(
                "vxi11 interrupt channel to {host}:{} refused: {e}",
                parms.host_port
            );
            return ErrorCode::ChannelNotEstablished;
        }
    };
    let _ = stream.set_nodelay(true);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let sender = tokio::spawn(async move {
        let mut stream = BufStream::new(stream);
        let mut xid: u32 = 0;
        while let Some(handle) = rx.recv().await {
            xid = xid.wrapping_add(1);
            let call = rpc::encode_call(
                xid,
                DEVICE_INTR_PROG,
                DEVICE_INTR_VERS,
                DEVICE_INTR_SRQ,
                &DeviceSrqParms { handle }.encode(),
            );
            if rpc::write_record(&mut stream, &call).await.is_err() {
                return;
            }
            // device_intr_srq is void, but ONC-RPC still answers it and the
            // reply must not rot in the socket. A peer that treats it as
            // one-way and answers nothing just costs this short wait.
            let _ = tokio::time::timeout(
                Duration::from_millis(1000),
                rpc::read_record(&mut stream, 4096),
            )
            .await;
        }
    });

    // The forwarder: watch the SRQ line, and when it rises, serial-poll the
    // instruments this connection's enabled links point at. RQS set means
    // that instrument asked for service: every enabled link on it gets a
    // device_intr_srq with its registered handle. The poll consumes RQS at
    // the instrument, so one bus event becomes one notification — the same
    // single-consumer caveat the HiSLIP front-end documents applies across
    // front-ends too: whichever forwarder polls first takes the byte.
    let subscriber = instrument_for(shared.config.default_pad);
    let owned = conn.owned.clone();
    let shutdown = Arc::new(Notify::new());
    {
        let shared = shared.clone();
        let shutdown = shutdown.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            debug!("vxi11 srq forwarder starting");
            // The broadcast carries edges; the line is a level. An SRQ
            // already asserted when this channel comes up — raised before
            // the client enabled interrupts, or orphaned at daemon startup —
            // would otherwise wait forever for an edge nobody will send.
            let starts_asserted = subscriber.srq_asserted().await.unwrap_or(false);
            let Some(mut srq) = subscriber.subscribe_srq().await else {
                // The adapter cannot report SRQ; without a signal to watch
                // there will never be anything to forward. The channel
                // stays up and stays silent, which RULE B.6.90 permits.
                return;
            };
            let mut initial_pass = starts_asserted;
            loop {
                use tokio::sync::broadcast::error::RecvError;
                if initial_pass {
                    initial_pass = false;
                } else {
                    tokio::select! {
                        _ = shutdown.notified() => {
                            debug!("vxi11 srq forwarder stopping");
                            return;
                        }
                        received = srq.recv() => match received {
                            // Lagged counts: SRQ is a level, a missed
                            // notification and a delivered one mean the same.
                            Ok(()) | Err(RecvError::Lagged(_)) => {}
                            Err(RecvError::Closed) => return,
                        }
                    }
                }
                debug!("vxi11 srq forwarder: line asserted");
                let targets: Vec<Arc<Link>> = {
                    let owned = owned.lock().unwrap();
                    let table = shared.links.lock().unwrap();
                    owned
                        .iter()
                        .filter_map(|lid| table.links.get(lid).cloned())
                        .filter(|l| l.srq_handle.lock().unwrap().is_some())
                        .collect()
                };
                // Device links are notified only when *their* instrument has
                // RQS set — one poll per instrument. This deliberately
                // diverges from VXI-11.2 RULE B.4.13, which notifies every
                // enabled link on any line edge: the filtered form is what
                // the bench validated (multidev isolation), and it spares
                // clients a spurious wake per unrelated instrument. An
                // interface link has no instrument to poll, so it gets the
                // spec behavior: the line itself is its signal.
                let mut polled: HashMap<String, bool> = HashMap::new();
                for link in targets {
                    let resource = link.resource();
                    let requesting = if link.is_interface() {
                        true
                    } else {
                        match polled.get(&resource) {
                            Some(&r) => r,
                            None => {
                                let r = match link.instrument.serial_poll().await {
                                    Ok(stb) => stb & STB_RQS != 0,
                                    Err(e) => {
                                        debug!("vxi11 srq poll failed: {e:#}");
                                        false
                                    }
                                };
                                polled.insert(resource.clone(), r);
                                r
                            }
                        }
                    };
                    debug!(
                        "vxi11 srq forwarder: link {} on {resource} requesting={requesting}",
                        link.lid
                    );
                    if requesting {
                        let handle = link.srq_handle.lock().unwrap().clone();
                        if let Some(handle) = handle {
                            let _ = tx.send(handle);
                        }
                    }
                }
            }
        })
    };
    conn.intr = Some(IntrChannel {
        shutdown,
        sender,
        tx,
    });
    debug!(
        "vxi11 interrupt channel established to {host}:{}",
        parms.host_port
    );
    ErrorCode::NoError
}

/// The lock gate every device I/O operation passes (RULES B.6.17/B.6.18 for
/// write, B.6.25/B.6.26 for read, and their generic-op kin): proceed when
/// the resource is free or ours; otherwise waitlock decides between waiting
/// out lock_timeout and an immediate 11.
async fn acquire_access(
    shared: &Shared,
    link: &Link,
    flags: u32,
    lock_timeout_ms: u32,
) -> Result<(), ErrorCode> {
    let locks = &shared.config.locks;
    let (resource, holder) = (link.resource(), link.holder());
    if locks.has_access(&resource, holder) {
        return Ok(());
    }
    if flags & OP_FLAG_WAITLOCK == 0 {
        return Err(ErrorCode::DeviceLockedByAnotherLink);
    }
    if locks
        .wait_for_access_timeout(
            &resource,
            holder,
            Duration::from_millis(u64::from(lock_timeout_ms)),
        )
        .await
    {
        Ok(())
    } else {
        Err(ErrorCode::DeviceLockedByAnotherLink)
    }
}

/// VXI-11.2 §B.5: the interface command set. Table B.1 fixes each command's
/// data_in length and datasize; a mismatch is error 5 before any action
/// (RULE B.5.3). Values are unsigned integers of datasize bytes in the
/// client's declared byte order (RULE B.5.4).
async fn device_docmd(shared: &Shared, parms: DeviceDocmdParms) -> DeviceDocmdResp {
    let fail = |error: ErrorCode| DeviceDocmdResp {
        error: error.as_u32(),
        data_out: Vec::new(),
    };
    let Some(link) = link_for(shared, parms.lid) else {
        return fail(ErrorCode::InvalidLinkIdentifier);
    };
    // RULE B.5.2: docmd on a device link is 8, no action.
    if !link.is_interface() {
        return fail(ErrorCode::OperationNotSupported);
    }
    if let Err(e) = acquire_access(shared, &link, parms.flags, parms.lock_timeout_ms).await {
        return fail(e);
    }

    /// Decode data_in as one unsigned integer per RULE B.5.4.
    fn uint_in(parms: &DeviceDocmdParms) -> u32 {
        let mut value: u32 = 0;
        let bytes: Box<dyn Iterator<Item = &u8>> = if parms.network_order {
            Box::new(parms.data_in.iter())
        } else {
            Box::new(parms.data_in.iter().rev())
        };
        for b in bytes {
            value = (value << 8) | u32::from(*b);
        }
        value
    }

    /// Encode one unsigned integer as datasize bytes per RULE B.5.4.
    fn uint_out(value: u32, datasize: usize, network_order: bool) -> Vec<u8> {
        let mut out: Vec<u8> = (0..datasize)
            .map(|i| (value >> (8 * (datasize - 1 - i))) as u8)
            .collect();
        if !network_order {
            out.reverse();
        }
        out
    }

    /// RULE B.5.3: the fixed length/datasize pairs from Table B.1.
    fn sizes_ok(
        parms: &DeviceDocmdParms,
        len: std::ops::RangeInclusive<usize>,
        datasize: i32,
    ) -> bool {
        len.contains(&parms.data_in.len()) && parms.datasize == datasize
    }

    let ok = |data_out: Vec<u8>| DeviceDocmdResp {
        error: ErrorCode::NoError.as_u32(),
        data_out,
    };
    let echo = |parms: &DeviceDocmdParms| ok(parms.data_in.clone());

    let window = OpWindow::open(&link);
    let result = match parms.cmd {
        // Send Command: raw command bytes under ATN (RULE B.5.5).
        0x020000 => {
            if !sizes_ok(&parms, 0..=128, 1) {
                return fail(ErrorCode::ParameterError);
            }
            let mut bus = link.instrument.hold().await;
            match bus.send_bus_command(&parms.data_in).await {
                Ok(()) => echo(&parms),
                Err(e) => {
                    debug!("docmd send-command failed: {e:#}");
                    fail(ErrorCode::IoError)
                }
            }
        }
        // Bus Status (RULE B.5.6, Table B.2).
        0x020001 => {
            if !sizes_ok(&parms, 2..=2, 2) {
                return fail(ErrorCode::ParameterError);
            }
            let selector = uint_in(&parms);
            let value = match bus_status(&link, selector).await {
                Ok(v) => v,
                Err(code) => return fail(code),
            };
            ok(uint_out(value, 2, parms.network_order))
        }
        // ATN Control (RULE B.5.7).
        0x020002 => {
            if !sizes_ok(&parms, 2..=2, 2) {
                return fail(ErrorCode::ParameterError);
            }
            let mut bus = link.instrument.hold().await;
            match bus.set_atn(uint_in(&parms) != 0).await {
                Ok(()) => echo(&parms),
                Err(e) => {
                    debug!("docmd ATN control failed: {e:#}");
                    // The 82357 backend has no verified raw-ATN path; its
                    // refusal surfaces here as 8, not as a fake success.
                    fail(ErrorCode::OperationNotSupported)
                }
            }
        }
        // REN Control (RULE B.5.8).
        0x020003 => {
            if !sizes_ok(&parms, 2..=2, 2) {
                return fail(ErrorCode::ParameterError);
            }
            match link.instrument.ren(uint_in(&parms) != 0).await {
                Ok(()) => echo(&parms),
                Err(e) => {
                    debug!("docmd REN control failed: {e:#}");
                    fail(ErrorCode::IoError)
                }
            }
        }
        // Pass Control (RULE B.5.9): this daemon is the one controller its
        // architecture supports; releasing CIC would strand every front-end.
        // Refused, and documented as a deliberate divergence (ROADMAP 8).
        0x020004 => {
            if !sizes_ok(&parms, 4..=4, 4) {
                return fail(ErrorCode::ParameterError);
            }
            fail(ErrorCode::OperationNotSupported)
        }
        // Bus Address (RULE B.5.10): re-address the controller. Out of
        // range is 5; the new address echoes back and Bus Status selector 8
        // reports it from then on.
        0x02000A => {
            if !sizes_ok(&parms, 4..=4, 4) {
                return fail(ErrorCode::ParameterError);
            }
            let pad = uint_in(&parms);
            if pad > 30 {
                return fail(ErrorCode::ParameterError);
            }
            let mut bus = link.instrument.hold().await;
            match bus.set_controller_pad(pad as u8).await {
                Ok(()) => echo(&parms),
                Err(e) => {
                    debug!("docmd bus-address set failed: {e:#}");
                    fail(ErrorCode::IoError)
                }
            }
        }
        // IFC Control (RULE B.5.11): datasize is unconstrained ("X"),
        // data_in must be empty, data_out returns empty.
        0x020010 => {
            if !parms.data_in.is_empty() {
                return fail(ErrorCode::ParameterError);
            }
            let mut bus = link.instrument.hold().await;
            match bus.ifc().await {
                Ok(()) => ok(Vec::new()),
                Err(e) => {
                    debug!("docmd IFC failed: {e:#}");
                    fail(ErrorCode::IoError)
                }
            }
        }
        // RULE B.5.1: anything else is 8.
        _ => fail(ErrorCode::OperationNotSupported),
    };
    if window.take_abort() {
        return fail(ErrorCode::Abort);
    }
    result
}

/// Bus Status selectors (Table B.2), answered from the live control lines
/// and the daemon's own state.
async fn bus_status(link: &Link, selector: u32) -> Result<u32, ErrorCode> {
    let live = |b: bool| u32::from(b);
    match selector {
        1..=3 => {
            let mut bus = link.instrument.hold().await;
            match bus.bus_lines().await {
                Ok(lines) => Ok(match selector {
                    1 => live(lines.ren),
                    2 => live(lines.srq),
                    _ => live(lines.ndac),
                }),
                Err(e) => {
                    debug!("docmd bus-status line read failed: {e:#}");
                    // A backend that cannot read the lines refuses; a made-up
                    // line level would be a plausible lie.
                    Err(ErrorCode::OperationNotSupported)
                }
            }
        }
        // System controller / controller-in-charge: this daemon is both, by
        // architecture, whenever it is running as a controller at all.
        4 | 5 => Ok(1),
        // Addressed to talk / listen: the daemon addresses transiently
        // inside bus transactions, and docmd itself holds the bus — so at
        // the moment this question is answerable, the answer is no.
        6 | 7 => Ok(0),
        8 => Ok(u32::from(link.instrument.hold().await.controller_pad())),
        _ => Err(ErrorCode::ParameterError),
    }
}

fn link_for(shared: &Shared, lid: i32) -> Option<Arc<Link>> {
    shared.links.lock().unwrap().links.get(&lid).cloned()
}

fn effective_timeout(config: &Config, io_timeout_ms: u32) -> u32 {
    if io_timeout_ms == 0 {
        config.default_io_timeout_ms
    } else {
        io_timeout_ms
    }
}

async fn device_write(shared: &Shared, parms: DeviceWriteParms) -> DeviceWriteResp {
    let Some(link) = link_for(shared, parms.lid) else {
        return DeviceWriteResp {
            error: ErrorCode::InvalidLinkIdentifier.as_u32(),
            size: 0,
        };
    };
    if let Err(e) = acquire_access(shared, &link, parms.flags, parms.lock_timeout_ms).await {
        return DeviceWriteResp {
            error: e.as_u32(),
            size: 0,
        };
    }
    let window = OpWindow::open(&link);
    let send_eoi = parms.flags & OP_FLAG_END != 0;
    // Interface links write with no addressing sequence: IEEE 488.2 16.2.3
    // SEND DATA BYTES (VXI-11.2 RULE B.4.2) — the client has done its own
    // addressing with docmd Send Command.
    let unaddressed = link.is_interface();

    // The io_timeout is a budget over the whole transfer (RULE B.6.19),
    // enforced here in chunks. A write chunk, unlike a read slice, must be
    // given a real chance to complete — a slow listener legitimately stalls
    // mid-handshake — so each chunk's adapter timeout is the *whole
    // remaining budget*, floored to an exact adapter timeout step so the
    // hardware never waits longer than the client allowed. (The NI table
    // rounds up otherwise: 1500 ms becomes a 3 s wait, and the reply loses
    // the race against the client's own io_timeout + grace deadline.)
    // Aborts land between chunks; the transferred count always goes back
    // (RULES B.6.20/B.6.21).
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(u64::from(effective_timeout(
            &shared.config,
            parms.io_timeout_ms,
        )));
    const WRITE_CHUNK: usize = 4096;
    let mut bus = link.instrument.hold().await;
    let mut written: usize = 0;
    let mut outcome = ErrorCode::NoError;
    let total = parms.data.len();
    // A zero-length write still runs once: OBSERVATION B.6.6 allows it (no
    // device action), and the END flag on an empty message is legal.
    loop {
        if window.take_abort() {
            outcome = ErrorCode::Abort;
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            outcome = ErrorCode::IoTimeout;
            break;
        }
        bus.set_timeout(adapter_floor_ms(remaining.as_millis() as u64));
        let end = (written + WRITE_CHUNK).min(total);
        let chunk = &parms.data[written..end];
        let last = end == total;
        let sent = if unaddressed {
            bus.send_data_unaddressed(chunk, send_eoi && last).await
        } else if written == 0 {
            bus.write(chunk, send_eoi && last).await
        } else {
            // Addressing happened with the first chunk and is sticky; the
            // rest of the message must not re-address (a listener would see
            // UNL mid-message).
            bus.send_data_unaddressed(chunk, send_eoi && last).await
        };
        match sent {
            Ok(()) => {
                written = end;
                if last {
                    break;
                }
            }
            Err(e) => {
                debug!("vxi11 device_write failed at byte {written}: {e:#}");
                // The backend reports failure as one error chain; a timeout
                // is distinguished by its message. Brittle in principle, but
                // the alternative is reporting every timeout as a device I/O
                // error, which RULE B.6.19 forbids.
                let timeout = format!("{e:#}").to_ascii_lowercase().contains("timeout");
                outcome = if timeout {
                    ErrorCode::IoTimeout
                } else {
                    ErrorCode::IoError
                };
                break;
            }
        }
    }
    bus.set_timeout(shared.config.default_io_timeout_ms);
    drop(bus);
    if outcome == ErrorCode::NoError && window.take_abort() {
        outcome = ErrorCode::Abort;
    }
    DeviceWriteResp {
        error: outcome.as_u32(),
        size: written as u32,
    }
}

/// The largest exact adapter-timeout step at or below `remaining_ms`. The
/// steps are the NI code table's 1-3-10 decade boundaries; the 82357 honors
/// milliseconds directly, where flooring costs one re-loop at most.
fn adapter_floor_ms(remaining_ms: u64) -> u32 {
    const STEPS: [u32; 12] = [
        300_000, 100_000, 30_000, 10_000, 3_000, 1_000, 300, 100, 30, 10, 3, 1,
    ];
    for step in STEPS {
        if u64::from(step) <= remaining_ms {
            return step;
        }
    }
    1
}

async fn device_read(shared: &Shared, parms: DeviceReadParms) -> DeviceReadResp {
    let fail = |error: ErrorCode| DeviceReadResp {
        error: error.as_u32(),
        reason: 0,
        data: Vec::new(),
    };
    let Some(link) = link_for(shared, parms.lid) else {
        return fail(ErrorCode::InvalidLinkIdentifier);
    };
    // RULE B.6.23.1.b: requestSize zero terminates immediately with REQCNT —
    // zero bytes were requested and zero delivered.
    if parms.request_size == 0 {
        return DeviceReadResp {
            error: ErrorCode::NoError.as_u32(),
            reason: RX_REQCNT,
            data: Vec::new(),
        };
    }
    if let Err(e) = acquire_access(shared, &link, parms.flags, parms.lock_timeout_ms).await {
        return fail(e);
    }
    let termchr = (parms.flags & OP_FLAG_TERMCHRSET != 0).then_some(parms.term_char as u8);
    let clamped = (parms.request_size as usize).min(MAX_READ);

    let window = OpWindow::open(&link);
    let mut bus = link.instrument.hold().await;
    // The terminator is per-operation in VXI-11 but persistent bus state to
    // the backend (and to the Prologix front-end's clients): save, set what
    // this read needs — enabled iff termchrset, off otherwise so another
    // front-end's terminator cannot truncate this read — and restore.
    let saved_eos = bus.eos();
    match termchr {
        Some(ch) => bus.set_eos(ch, true),
        None => bus.set_eos(saved_eos.0, false),
    }

    // The io_timeout deadline is enforced *here*, with the bus polled in
    // short slices, rather than handed to the adapter whole. Two reasons.
    // Adapter timeouts are a coarse code table that rounds up — 1500 ms
    // becomes a 3 s bus wait — and a VXI-11 client only grants the server
    // io_timeout plus a small grace to answer (pyvisa-py: +1 s), so a
    // rounded-up wait makes an honest timeout reply arrive after the client
    // has already declared the connection dead. And a bus transaction, once
    // started, cannot be safely abandoned mid-flight; short slices mean
    // there is always one about to finish. Data arriving mid-slice returns
    // promptly; an instrument that pauses between slices just gets
    // re-addressed, which is what any controller's retrying read does.
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(u64::from(effective_timeout(
            &shared.config,
            parms.io_timeout_ms,
        )));
    // Slice per the adapter's requirement: coarse-timeout hardware is
    // polled; exact-timeout hardware gets the whole remaining budget in one
    // read, because its timeout path may be heavyweight (the 82357 answers
    // a timed-out read by aborting the transfer and pulsing IFC — fine
    // when the budget is truly spent, ruinous every 250 ms).
    let slice_ms = bus.read_slice_ms();
    let mut data: Vec<u8> = Vec::new();
    enum ReadEnd {
        Terminated(u32),
        Deadline,
        Aborted,
        Bus(anyhow::Error),
    }
    let outcome = loop {
        // RULE B.6.29: abort terminates the read — checked between slices,
        // never inside one, so the bus transaction underway completes.
        if window.take_abort() {
            break ReadEnd::Aborted;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break ReadEnd::Deadline;
        }
        let slice = match slice_ms {
            Some(s) => (remaining.as_millis() as u32).clamp(1, s),
            None => (remaining.as_millis() as u32).max(1),
        };
        bus.set_timeout(slice);
        let attempt_started = tokio::time::Instant::now();
        let attempt = if link.is_interface() {
            // RULE B.4.4: RECEIVE RESPONSE MESSAGE, no addressing — the
            // client established the talker itself.
            bus.read_unaddressed(clamped - data.len()).await
        } else {
            bus.read(clamped - data.len()).await
        };
        match attempt {
            Ok((chunk, end)) => {
                // An empty attempt that returned well inside its slice — an
                // adapter fast-failing, or a test double answering from
                // memory — must not turn this loop into a spin: pace it out
                // to the slice. This is also the loop's only guaranteed
                // await point, and the abort check above depends on the
                // executor getting a turn.
                if chunk.is_empty() {
                    let spent = attempt_started.elapsed();
                    let slice_len = std::time::Duration::from_millis(u64::from(slice));
                    if spent < slice_len {
                        tokio::time::sleep(slice_len - spent).await;
                    }
                }
                data.extend_from_slice(&chunk);
                let mut reason = 0;
                if end {
                    reason |= RX_END;
                }
                if let Some(ch) = termchr {
                    if data.last() == Some(&ch) {
                        reason |= RX_CHR;
                    }
                }
                if data.len() == parms.request_size as usize {
                    reason |= RX_REQCNT;
                }
                if reason != 0 || data.len() == clamped {
                    // Terminated per RULE B.6.23 — or condition d, our
                    // buffer filled with reason 0, and the client comes
                    // back for the rest (OBSERVATION B.6.9).
                    break ReadEnd::Terminated(reason);
                }
            }
            Err(e) => {
                // A backend that reports its timeout as an error (the 82357)
                // is saying "nothing arrived", not "the bus broke": treat it
                // as deadline progress, not as I/O failure.
                if format!("{e:#}").to_ascii_lowercase().contains("timed out")
                    || format!("{e:#}").to_ascii_lowercase().contains("timeout")
                {
                    // Pace a backend that fast-fails its timeouts so this
                    // cannot spin; a real adapter consumed the wait already.
                    let spent = attempt_started.elapsed();
                    let floor = std::time::Duration::from_millis(50);
                    if spent < floor {
                        tokio::time::sleep(floor - spent).await;
                    }
                    continue;
                }
                break ReadEnd::Bus(e);
            }
        }
    };
    bus.set_eos(saved_eos.0, saved_eos.1);
    bus.set_timeout(shared.config.default_io_timeout_ms);
    drop(bus);

    match outcome {
        ReadEnd::Terminated(reason) => DeviceReadResp {
            error: ErrorCode::NoError.as_u32(),
            reason,
            data,
        },
        // No termination condition before the deadline. RULE B.6.27:
        // error 15, the partial data included, reason 0.
        ReadEnd::Deadline => DeviceReadResp {
            error: ErrorCode::IoTimeout.as_u32(),
            reason: 0,
            data,
        },
        // RULE B.6.29/B.6.30: error 23, and the bytes so far still go back.
        ReadEnd::Aborted => DeviceReadResp {
            error: ErrorCode::Abort.as_u32(),
            reason: 0,
            data,
        },
        ReadEnd::Bus(e) => {
            debug!("vxi11 device_read failed: {e:#}");
            fail(ErrorCode::IoError)
        }
    }
}

async fn device_readstb(shared: &Shared, parms: DeviceGenericParms) -> DeviceReadStbResp {
    let Some(link) = link_for(shared, parms.lid) else {
        return DeviceReadStbResp {
            error: ErrorCode::InvalidLinkIdentifier.as_u32(),
            stb: 0,
        };
    };
    if link.is_interface() {
        // VXI-11.2 defines readstb only for device links (B.4.7 has no
        // interface rule); a status byte of "the interface" is not a thing.
        return DeviceReadStbResp {
            error: ErrorCode::OperationNotSupported.as_u32(),
            stb: 0,
        };
    }
    if let Err(e) = acquire_access(shared, &link, parms.flags, parms.lock_timeout_ms).await {
        return DeviceReadStbResp {
            error: e.as_u32(),
            stb: 0,
        };
    }
    let window = OpWindow::open(&link);
    let mut bus = link.instrument.hold().await;
    bus.set_timeout(effective_timeout(&shared.config, parms.io_timeout_ms));
    let polled = bus.serial_poll().await;
    bus.set_timeout(shared.config.default_io_timeout_ms);
    drop(bus);
    if window.take_abort() {
        return DeviceReadStbResp {
            error: ErrorCode::Abort.as_u32(),
            stb: 0,
        };
    }
    match polled {
        Ok(stb) => DeviceReadStbResp {
            error: ErrorCode::NoError.as_u32(),
            stb,
        },
        Err(e) => {
            debug!("vxi11 device_readstb failed: {e:#}");
            DeviceReadStbResp {
                error: ErrorCode::IoError.as_u32(),
                stb: 0,
            }
        }
    }
}

async fn device_simple(shared: &Shared, proc: u32, parms: DeviceGenericParms) -> u32 {
    let Some(link) = link_for(shared, parms.lid) else {
        return ErrorCode::InvalidLinkIdentifier.as_u32();
    };
    if let Err(e) = acquire_access(shared, &link, parms.flags, parms.lock_timeout_ms).await {
        return e.as_u32();
    }
    let window = OpWindow::open(&link);
    let instr = &link.instrument;
    let outcome = if link.is_interface() {
        match proc {
            // RULE B.4.8: GET without addressing — every currently-addressed
            // listener triggers.
            DEVICE_TRIGGER => {
                let mut bus = instr.hold().await;
                bus.send_bus_command(&[GPIB_GET]).await
            }
            // RULE B.4.6: bus-wide DCL, all devices.
            DEVICE_CLEAR => {
                let mut bus = instr.hold().await;
                bus.send_bus_command(&[GPIB_DCL]).await
            }
            // RULES B.4.10/B.4.12: remote and local are per-device notions;
            // the interface link answers 8.
            DEVICE_REMOTE | DEVICE_LOCAL => {
                return ErrorCode::OperationNotSupported.as_u32();
            }
            _ => unreachable!("dispatch sends only the four generic procedures"),
        }
    } else {
        match proc {
            DEVICE_TRIGGER => instr.trigger().await,
            DEVICE_CLEAR => instr.device_clear().await,
            // B.6.8/B.6.9 of VXI-11.3 map these onto REN addressing; the
            // instrument methods drive REN plus addressing, matching what the
            // HiSLIP front-end does for its remote/local operations.
            DEVICE_REMOTE => instr.go_to_remote().await,
            DEVICE_LOCAL => instr.go_to_local().await,
            _ => unreachable!("dispatch sends only the four generic procedures"),
        }
    };
    if window.take_abort() {
        return ErrorCode::Abort.as_u32();
    }
    match outcome {
        Ok(()) => ErrorCode::NoError.as_u32(),
        Err(e) => {
            debug!("vxi11 generic op {proc} failed: {e:#}");
            ErrorCode::IoError.as_u32()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_resolve_to_pads() {
        assert_eq!(parse_device_name("gpib0,18", 5), Ok(DeviceAddr::Pad(18)));
        assert_eq!(parse_device_name("gpib0,0", 5), Ok(DeviceAddr::Pad(0)));
        assert_eq!(parse_device_name("gpib0,30", 5), Ok(DeviceAddr::Pad(30)));
        assert_eq!(parse_device_name("GPIB0,18", 5), Ok(DeviceAddr::Pad(18)));
        assert_eq!(parse_device_name(" inst0 ", 5), Ok(DeviceAddr::Pad(5)));
    }

    /// VXI-11.2 RULE B.1.3: a bare interface name is the interface itself.
    #[test]
    fn a_bare_gpib0_is_the_interface() {
        assert_eq!(parse_device_name("gpib0", 5), Ok(DeviceAddr::Interface));
    }

    #[test]
    fn a_pad_past_thirty_is_an_invalid_address() {
        // Table B.4: 21. GPIB primary addresses end at 30.
        assert_eq!(
            parse_device_name("gpib0,31", 5),
            Err(ErrorCode::InvalidAddress)
        );
    }

    #[test]
    fn a_secondary_address_is_refused_not_swallowed() {
        assert_eq!(
            parse_device_name("gpib0,18,96", 5),
            Err(ErrorCode::InvalidAddress)
        );
    }

    #[test]
    fn another_interface_number_is_not_accessible() {
        assert_eq!(
            parse_device_name("gpib1,18", 5),
            Err(ErrorCode::DeviceNotAccessible)
        );
    }

    #[test]
    fn garbage_names_are_syntax_errors() {
        for name in ["", "instr0", "usb0", "gpib0,", "gpib0,abc", "gpib,5"] {
            assert_eq!(
                parse_device_name(name, 5),
                Err(ErrorCode::SyntaxError),
                "{name:?}"
            );
        }
    }
}
