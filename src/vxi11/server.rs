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
// Not yet here, refused honestly rather than stubbed: locking (device_lock
// answers 8, operation not supported; create_link with lockDevice set is
// likewise refused — granting a lock nothing enforces would be a plausible
// lie), the interrupt channel (8), device_docmd (8), and the abort channel
// (abortPort is reported as 0, where a conforming client's connect fails
// loudly). Each lands in its own phase of docs/VXI11-PLAN.md.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::BufStream;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_recv_size: 65536,
            max_links: 64,
            default_io_timeout_ms: 3000,
            default_pad: 0,
        }
    }
}

/// Parse a create_link device name to a GPIB primary address.
///
/// Accepted spellings (VXI-11.2 §B.1): `gpib0,<pad>` addresses a device on
/// the one bus this daemon controls; `inst0` and a bare `gpib0` mean the
/// daemon's default PAD, matching the HiSLIP sub-address convention.
/// Errors are create_link's own table (B.4): a secondary address parses but
/// is refused with 21 until the backend can address one — not silently
/// dropped; an interface number other than 0 is 3, device not accessible
/// (the daemon has one bus); anything unparseable is 1, syntax error.
pub fn parse_device_name(name: &str, default_pad: u8) -> Result<u8, ErrorCode> {
    let name = name.trim().to_ascii_lowercase();
    if name == "inst0" || name == "gpib0" {
        return Ok(default_pad);
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
        None => Ok(pad),
    }
}

struct Link {
    instrument: Arc<Instrument>,
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
}

/// Server entry point. `instrument_for` maps a parsed PAD to the instrument
/// handle serving it.
pub async fn run<F>(listener: TcpListener, config: Config, instrument_for: F) -> io::Result<()>
where
    F: Fn(u8) -> Arc<Instrument> + Send + Sync + 'static,
{
    info!("VXI-11 core listening on {}", listener.local_addr()?);
    let shared = Arc::new(Shared {
        config,
        links: Mutex::new(LinkTable::default()),
        next_lid: AtomicI32::new(1),
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

async fn serve_connection<F>(
    stream: tokio::net::TcpStream,
    shared: Arc<Shared>,
    instrument_for: Arc<F>,
) -> anyhow::Result<()>
where
    F: Fn(u8) -> Arc<Instrument> + Send + Sync + 'static,
{
    let mut stream = BufStream::new(stream);
    // Links created on this connection, destroyed with it: the channel
    // closing is how a crashed client's links are recovered (§B.2).
    let mut owned: Vec<i32> = Vec::new();
    let result = loop {
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
                dispatch(&shared, instrument_for.as_ref(), &mut owned, header, args)
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
    let mut links = shared.links.lock().unwrap();
    for lid in owned {
        links.links.remove(&lid);
    }
    result
}

/// Dispatch one call. `Err(xid)` means the arguments did not decode —
/// GARBAGE_ARGS per RFC 5531, since the procedure was reachable but its
/// parameters were not.
async fn dispatch<F>(
    shared: &Shared,
    instrument_for: &F,
    owned: &mut Vec<i32>,
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
            create_link(shared, instrument_for, owned, parms).encode()
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
            let mut links = shared.links.lock().unwrap();
            // RULE B.6.10: an unknown lid is 4. RULE B.6.11: destroying a
            // link touches no device state, so there is no bus traffic here.
            let error = if links.links.remove(&lid).is_some() {
                owned.retain(|&l| l != lid);
                ErrorCode::NoError
            } else {
                ErrorCode::InvalidLinkIdentifier
            };
            encode_device_error(error.as_u32())
        }
        DEVICE_LOCK | DEVICE_ENABLE_SRQ | CREATE_INTR_CHAN | DESTROY_INTR_CHAN => {
            // Honest refusals for later phases (locking, SRQ). The argument
            // is not even decoded: the answer does not depend on it.
            encode_device_error(ErrorCode::OperationNotSupported.as_u32())
        }
        DEVICE_UNLOCK => {
            // No locks exist yet, so no link holds one: 12 is the truthful
            // answer the spec defines, not a stub.
            encode_device_error(ErrorCode::NoLockHeldByThisLink.as_u32())
        }
        DEVICE_DOCMD => {
            // Refused per-phase like the above, but with docmd's own
            // response shape so a conforming client can decode it.
            DeviceDocmdResp {
                error: ErrorCode::OperationNotSupported.as_u32(),
                data_out: Vec::new(),
            }
            .encode()
        }
        _ => return Ok(rpc::reply_proc_unavail(xid)),
    };
    Ok(rpc::reply_success(xid, &results))
}

fn create_link<F>(
    shared: &Shared,
    instrument_for: &F,
    owned: &mut Vec<i32>,
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
    if parms.lock_device {
        // Phase 4 brings locking; granting a lock nothing enforces would be
        // worse than refusing one.
        return refuse(ErrorCode::OperationNotSupported);
    }
    let name = String::from_utf8_lossy(&parms.device);
    let pad = match parse_device_name(&name, shared.config.default_pad) {
        Ok(pad) => pad,
        Err(e) => {
            debug!("create_link refused for device name {name:?}: {e:?}");
            return refuse(e);
        }
    };
    let instrument = instrument_for(pad);
    let mut links = shared.links.lock().unwrap();
    if links.links.len() >= shared.config.max_links {
        return refuse(ErrorCode::OutOfResources);
    }
    let lid = shared.next_lid.fetch_add(1, Ordering::Relaxed);
    links.links.insert(lid, Arc::new(Link { instrument }));
    owned.push(lid);
    debug!("vxi11 link {lid} created for {name:?}");
    CreateLinkResp {
        error: ErrorCode::NoError.as_u32(),
        lid,
        // Phase 4 starts the abort listener; a conforming client that tries
        // to abort meanwhile fails to connect, loudly.
        abort_port: 0,
        max_recv_size: shared.config.max_recv_size,
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
    let send_eoi = parms.flags & OP_FLAG_END != 0;
    let mut bus = link.instrument.hold().await;
    bus.set_timeout(effective_timeout(&shared.config, parms.io_timeout_ms));
    let written = bus.write(&parms.data, send_eoi).await;
    bus.set_timeout(shared.config.default_io_timeout_ms);
    match written {
        Ok(()) => DeviceWriteResp {
            error: ErrorCode::NoError.as_u32(),
            size: parms.data.len() as u32,
        },
        Err(e) => {
            debug!("vxi11 device_write failed: {e:#}");
            // The backend reports failure as one error chain; a timeout is
            // distinguished by its message. Brittle in principle, but the
            // alternative is reporting every timeout as a device I/O error,
            // which RULE B.6.19 forbids.
            let timeout = format!("{e:#}").to_ascii_lowercase().contains("timeout");
            DeviceWriteResp {
                error: if timeout {
                    ErrorCode::IoTimeout.as_u32()
                } else {
                    ErrorCode::IoError.as_u32()
                },
                size: 0,
            }
        }
    }
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
    let termchr = (parms.flags & OP_FLAG_TERMCHRSET != 0).then_some(parms.term_char as u8);
    let clamped = (parms.request_size as usize).min(MAX_READ);

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
    const SLICE_MS: u32 = 250;
    let mut data: Vec<u8> = Vec::new();
    let outcome = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break Ok(0); // deadline passed with no termination condition
        }
        let slice = (remaining.as_millis() as u32).clamp(1, SLICE_MS);
        bus.set_timeout(slice);
        match bus.read(clamped - data.len()).await {
            Ok((chunk, end)) => {
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
                    break Ok(reason);
                }
            }
            Err(e) => break Err(e),
        }
    };
    bus.set_eos(saved_eos.0, saved_eos.1);
    bus.set_timeout(shared.config.default_io_timeout_ms);
    drop(bus);

    match outcome {
        Ok(0) if data.len() < clamped => {
            // No termination condition before the deadline. RULE B.6.27:
            // error 15, the partial data included, reason 0.
            DeviceReadResp {
                error: ErrorCode::IoTimeout.as_u32(),
                reason: 0,
                data,
            }
        }
        Ok(reason) => DeviceReadResp {
            error: ErrorCode::NoError.as_u32(),
            reason,
            data,
        },
        Err(e) => {
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
    let mut bus = link.instrument.hold().await;
    bus.set_timeout(effective_timeout(&shared.config, parms.io_timeout_ms));
    let polled = bus.serial_poll().await;
    bus.set_timeout(shared.config.default_io_timeout_ms);
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
    let instr = &link.instrument;
    let outcome = match proc {
        DEVICE_TRIGGER => instr.trigger().await,
        DEVICE_CLEAR => instr.device_clear().await,
        // B.6.8/B.6.9 of VXI-11.3 map these onto REN addressing; the
        // instrument methods drive REN plus addressing, matching what the
        // HiSLIP front-end does for its remote/local operations.
        DEVICE_REMOTE => instr.go_to_remote().await,
        DEVICE_LOCAL => instr.go_to_local().await,
        _ => unreachable!("dispatch sends only the four generic procedures"),
    };
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
        assert_eq!(parse_device_name("gpib0,18", 5), Ok(18));
        assert_eq!(parse_device_name("gpib0,0", 5), Ok(0));
        assert_eq!(parse_device_name("gpib0,30", 5), Ok(30));
        assert_eq!(parse_device_name("GPIB0,18", 5), Ok(18));
        assert_eq!(parse_device_name(" inst0 ", 5), Ok(5));
        assert_eq!(parse_device_name("gpib0", 5), Ok(5));
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
