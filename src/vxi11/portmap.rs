// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Portmap v2 codec (RFC 1833 §3). Codec only: the optional responder that
// serves it arrives with the portmapper phase (docs/VXI11-PLAN.md phase 7).
//
// The portmapper exists here for one reason: commercial VISA stacks
// (NI, Keysight) hardwire a GETPORT lookup to find the VXI-11 core channel
// and offer no way to specify the port. pyvisa-py needs none of this — its
// resource syntax carries the port directly.

use super::xdr::{self, Cursor, XdrError};

/// RFC 1833 §3.1.
pub const PMAP_PROG: u32 = 100000;
pub const PMAP_VERS: u32 = 2;
pub const PMAP_PORT: u16 = 111;

pub const PMAPPROC_NULL: u32 = 0;
pub const PMAPPROC_SET: u32 = 1;
pub const PMAPPROC_UNSET: u32 = 2;
pub const PMAPPROC_GETPORT: u32 = 3;
pub const PMAPPROC_DUMP: u32 = 4;
pub const PMAPPROC_CALLIT: u32 = 5;

pub const IPPROTO_TCP: u32 = 6;
pub const IPPROTO_UDP: u32 = 17;

/// One (program, version, protocol) → port entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub prog: u32,
    pub vers: u32,
    pub prot: u32,
    /// `unsigned int` on the wire, though only 16 bits are meaningful.
    pub port: u32,
}

impl Mapping {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        xdr::put_u32(buf, self.prog);
        xdr::put_u32(buf, self.vers);
        xdr::put_u32(buf, self.prot);
        xdr::put_u32(buf, self.port);
    }

    pub fn decode(c: &mut Cursor<'_>) -> Result<Self, XdrError> {
        Ok(Self {
            prog: c.u32()?,
            vers: c.u32()?,
            prot: c.u32()?,
            port: c.u32()?,
        })
    }
}

/// GETPORT's result: the port, or 0 for "not registered" (RFC 1833 §3.2).
pub fn encode_getport_reply(port: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4);
    xdr::put_u32(&mut buf, port);
    buf
}

/// DUMP's result: `pmaplist`, an XDR optional-data chain — each entry
/// prefixed by bool TRUE, the end marked by bool FALSE (RFC 4506 §4.19's
/// linked-list convention, spelled `struct *pmaplist` in RFC 1833 §3.1).
pub fn encode_dump_reply(mappings: &[Mapping]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + mappings.len() * 20);
    for m in mappings {
        xdr::put_bool(&mut buf, true);
        m.encode(&mut buf);
    }
    xdr::put_bool(&mut buf, false);
    buf
}

/// Decode a DUMP reply. The client half, for tests and `rpcinfo`-style
/// diagnostics.
pub fn decode_dump_reply(data: &[u8]) -> Result<Vec<Mapping>, XdrError> {
    let mut c = Cursor::new(data);
    let mut mappings = Vec::new();
    while c.bool()? {
        mappings.push(Mapping::decode(&mut c)?);
    }
    Ok(mappings)
}

/// One bound portmap responder: the same fixed mapping table served over
/// TCP (record-marked) and UDP (bare datagrams — ONC-RPC over UDP has no
/// record marking).
///
/// This exists for VISA stacks that hardwire portmap discovery (NI,
/// Keysight) and for `rpcinfo -p`. pyvisa-py needs none of it: the
/// resource string carries the port. The table is fixed at startup —
/// PMAPPROC_SET/UNSET answer FALSE, the same refusal rpcbind gives
/// non-local callers, because a daemon that lets network peers rebind its
/// programs is an open relay. PMAPPROC_CALLIT is answered with silence:
/// RFC 1833 §3.2 sends no reply when the target program is unreachable,
/// and CALLIT forwards over UDP only, where nothing here is registered.
pub async fn run(
    tcp: tokio::net::TcpListener,
    udp: tokio::net::UdpSocket,
    mappings: Vec<Mapping>,
) -> std::io::Result<()> {
    use super::rpc;
    use std::sync::Arc;
    use tracing::{debug, info, warn};

    let mappings = Arc::new(mappings);
    info!(
        "portmap listening on {} (tcp) / {} (udp), {} registrations",
        tcp.local_addr()?,
        udp.local_addr()?,
        mappings.len()
    );

    /// Dispatch one call. `None` means "send nothing" (CALLIT failure).
    fn dispatch(record: &[u8], mappings: &[Mapping]) -> Option<Vec<u8>> {
        let (header, args) = match rpc::decode_call(record) {
            Ok(ok) => ok,
            Err(rpc::CallError::RpcVersion { xid, .. }) => {
                return Some(rpc::reply_rpc_mismatch(xid));
            }
            Err(e) => {
                debug!("portmap: {e}");
                return None;
            }
        };
        let xid = header.xid;
        if header.prog != PMAP_PROG {
            return Some(rpc::reply_prog_unavail(xid));
        }
        if header.vers != PMAP_VERS {
            return Some(rpc::reply_prog_mismatch(xid, PMAP_VERS, PMAP_VERS));
        }
        match header.proc {
            PMAPPROC_NULL => Some(rpc::reply_success(xid, &[])),
            PMAPPROC_GETPORT => {
                let mut c = Cursor::new(args);
                let Ok(wanted) = Mapping::decode(&mut c) else {
                    return Some(rpc::reply_garbage_args(xid));
                };
                // RFC 1833 §3.2: port 0 means "not registered". The caller's
                // port field is ignored on lookup.
                let port = mappings
                    .iter()
                    .find(|m| {
                        m.prog == wanted.prog && m.vers == wanted.vers && m.prot == wanted.prot
                    })
                    .map_or(0, |m| m.port);
                Some(rpc::reply_success(xid, &encode_getport_reply(port)))
            }
            PMAPPROC_DUMP => Some(rpc::reply_success(xid, &encode_dump_reply(mappings))),
            PMAPPROC_SET | PMAPPROC_UNSET => {
                // bool FALSE: the table is fixed (see above).
                let mut out = Vec::with_capacity(4);
                xdr::put_bool(&mut out, false);
                Some(rpc::reply_success(xid, &out))
            }
            PMAPPROC_CALLIT => None,
            _ => Some(rpc::reply_proc_unavail(xid)),
        }
    }

    let udp_mappings = mappings.clone();
    let udp_task = async move {
        let mut buf = vec![0u8; 4096];
        loop {
            let (len, peer) = match udp.recv_from(&mut buf).await {
                Ok(ok) => ok,
                Err(e) => {
                    warn!("portmap udp receive failed: {e}");
                    continue;
                }
            };
            if let Some(reply) = dispatch(&buf[..len], &udp_mappings) {
                let _ = udp.send_to(&reply, peer).await;
            }
        }
    };

    let tcp_task = async move {
        loop {
            let Ok((stream, _peer)) = tcp.accept().await else {
                continue;
            };
            let mappings = mappings.clone();
            tokio::spawn(async move {
                let mut stream = tokio::io::BufStream::new(stream);
                loop {
                    let record = match rpc::read_record(&mut stream, 4096).await {
                        Ok(Some(r)) => r,
                        _ => return,
                    };
                    match dispatch(&record, &mappings) {
                        Some(reply) => {
                            if rpc::write_record(&mut stream, &reply).await.is_err() {
                                return;
                            }
                        }
                        None => return,
                    }
                }
            });
        }
    };

    tokio::join!(udp_task, tcp_task);
    unreachable!("both portmap loops run forever");
}

/// Register (or unregister) a mapping with a portmapper — normally the
/// system rpcbind on localhost. PMAPPROC_SET/UNSET over UDP; rpcbind
/// accepts these from local callers, which is the classic way an ONC-RPC
/// service announces itself (this is what NFS does). Cooperating beats the
/// alternative: two daemons fighting over port 111 with systemd Conflicts=
/// turns into a restart war, as observed in the field.
///
/// Returns whether the portmapper said yes — FALSE is an answer (rpcbind
/// refuses non-local or conflicting registrations), not a transport error.
pub async fn set_registration(
    host: &str,
    port: u16,
    mapping: Mapping,
    register: bool,
) -> anyhow::Result<bool> {
    use super::rpc;
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    sock.connect((host, port)).await?;
    let mut args = Vec::new();
    mapping.encode(&mut args);
    let proc = if register {
        PMAPPROC_SET
    } else {
        PMAPPROC_UNSET
    };
    let call = rpc::encode_call(1, PMAP_PROG, PMAP_VERS, proc, &args);

    // UDP: retry a couple of times before concluding nobody is home.
    let mut buf = vec![0u8; 512];
    for _attempt in 0..3 {
        sock.send(&call).await?;
        match tokio::time::timeout(std::time::Duration::from_millis(1000), sock.recv(&mut buf))
            .await
        {
            Ok(len) => {
                let len = len?;
                return match rpc::decode_reply(&buf[..len], 1) {
                    Ok(rpc::ReplyBody::Success(body)) => {
                        let mut c = Cursor::new(body);
                        Ok(c.bool()?)
                    }
                    Ok(other) => anyhow::bail!("portmapper answered {other:?}"),
                    Err(e) => anyhow::bail!("bad portmapper reply: {e}"),
                };
            }
            Err(_) => continue,
        }
    }
    anyhow::bail!("no portmapper answered at {host}:{port} after 3 attempts")
}

/// Is a portmapper answering at `host:port`? One NULL call, short timeout.
/// Probing also wakes a socket-activated rpcbind, which is exactly right:
/// if the system *would* run rpcbind on demand, cooperation is the mode.
pub async fn probe(host: &str, port: u16) -> bool {
    use super::rpc;
    let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else {
        return false;
    };
    if sock.connect((host, port)).await.is_err() {
        return false;
    }
    let call = rpc::encode_call(2, PMAP_PROG, PMAP_VERS, PMAPPROC_NULL, &[]);
    let mut buf = [0u8; 128];
    for _ in 0..2 {
        if sock.send(&call).await.is_err() {
            return false;
        }
        if let Ok(Ok(len)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), sock.recv(&mut buf)).await
        {
            return rpc::decode_reply(&buf[..len], 2).is_ok();
        }
    }
    false
}

/// Look up one mapping (GETPORT) — the self-check of register mode.
pub async fn getport(host: &str, port: u16, mapping: Mapping) -> anyhow::Result<u32> {
    use super::rpc;
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    sock.connect((host, port)).await?;
    let mut args = Vec::new();
    mapping.encode(&mut args);
    let call = rpc::encode_call(3, PMAP_PROG, PMAP_VERS, PMAPPROC_GETPORT, &args);
    let mut buf = [0u8; 128];
    sock.send(&call).await?;
    let len = tokio::time::timeout(std::time::Duration::from_millis(1000), sock.recv(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("portmapper did not answer GETPORT"))??;
    match rpc::decode_reply(&buf[..len], 3) {
        Ok(rpc::ReplyBody::Success(body)) => Ok(Cursor::new(body).u32()?),
        other => anyhow::bail!("bad GETPORT reply: {other:?}"),
    }
}

/// Register mode's resident half: keep the registration alive against the
/// portmapper at `host:port` until cancelled. An rpcbind restart wipes its
/// table silently; the periodic self-check notices and re-registers, which
/// is sturdier than tying unit lifecycles together.
pub async fn maintain_registration(host: &str, port: u16, mapping: Mapping) -> ! {
    use tracing::{info, warn};
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match getport(host, port, mapping).await {
            Ok(p) if p == mapping.port => {}
            Ok(_) => match set_registration(host, port, mapping, true).await {
                Ok(true) => info!("re-registered with the portmapper (its table was reset)"),
                Ok(false) => warn!("portmapper refuses re-registration; discovery is down"),
                Err(e) => warn!("portmapper unreachable for re-registration: {e:#}"),
            },
            Err(e) => warn!("portmapper self-check failed: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mapping_is_four_words() {
        let m = Mapping {
            prog: 0x0607AF,
            vers: 1,
            prot: IPPROTO_TCP,
            port: 9010,
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(buf.len(), 16);
        assert_eq!(Mapping::decode(&mut Cursor::new(&buf)).unwrap(), m);
    }

    #[test]
    fn dump_chains_entries_with_the_optional_data_bool() {
        let list = [
            Mapping {
                prog: 0x0607AF,
                vers: 1,
                prot: IPPROTO_TCP,
                port: 9010,
            },
            Mapping {
                prog: 0x0607B0,
                vers: 1,
                prot: IPPROTO_TCP,
                port: 9011,
            },
        ];
        let encoded = encode_dump_reply(&list);
        // 2 × (bool + mapping) + terminating bool.
        assert_eq!(encoded.len(), 2 * 20 + 4);
        assert_eq!(decode_dump_reply(&encoded).unwrap(), list);
    }

    #[test]
    fn an_empty_dump_is_just_the_terminator() {
        assert_eq!(encode_dump_reply(&[]), [0, 0, 0, 0]);
        assert_eq!(decode_dump_reply(&[0, 0, 0, 0]).unwrap(), []);
    }

    #[test]
    fn an_unterminated_dump_chain_is_truncated_not_accepted() {
        let mut encoded = encode_dump_reply(&[Mapping {
            prog: 1,
            vers: 2,
            prot: IPPROTO_UDP,
            port: 3,
        }]);
        encoded.truncate(encoded.len() - 4); // drop the terminating FALSE
        assert_eq!(decode_dump_reply(&encoded), Err(XdrError::Truncated));
    }
}
