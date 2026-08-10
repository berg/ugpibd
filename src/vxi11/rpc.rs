// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// ONC RPC v2 (RFC 5531): calls, replies, and the TCP record marking that
// frames them.
//
// This is a server that also needs the client half (the CLI, the tests, and
// the interrupt channel — where *we* are the RPC client calling the VISA
// client back), so both directions live here, sharing one encoding.
//
// Authentication is AUTH_NONE-shaped: whatever credential flavor a caller
// presents is accepted and its contents ignored, and replies always carry an
// AUTH_NONE verifier. RFC 5531 §10.1 makes null auth the one mandatory
// flavor; VXI-11 clients send AUTH_NONE or AUTH_SYS and expect nothing
// back. Refusing AUTH_SYS would lock out real VISA stacks to protect
// nothing — the daemon's actual access story is the bind address (see
// ROADMAP entry 1).

use std::fmt;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::xdr::{self, Cursor, XdrError};

/// RFC 5531 §9: `rpcvers` MUST be 2.
pub const RPC_VERSION: u32 = 2;

/// Longest credential/verifier body the protocol allows (RFC 5531 §8.2).
const AUTH_BODY_MAX: u32 = 400;

/// msg_type
const CALL: u32 = 0;
const REPLY: u32 = 1;

/// reply_stat
const MSG_ACCEPTED: u32 = 0;
const MSG_DENIED: u32 = 1;

/// accept_stat (RFC 5531 §9)
const SUCCESS: u32 = 0;
const PROG_UNAVAIL: u32 = 1;
const PROG_MISMATCH: u32 = 2;
const PROC_UNAVAIL: u32 = 3;
const GARBAGE_ARGS: u32 = 4;
const SYSTEM_ERR: u32 = 5;

/// reject_stat. AUTH_ERROR (1) exists in the spec but has no encoder here:
/// this server never rejects on auth (see module comment).
const RPC_MISMATCH: u32 = 0;

/// auth flavor
const AUTH_NONE: u32 = 0;

/// One decoded call header; the procedure-specific arguments follow it in
/// the record and are handed over undecoded, because only the program
/// dispatch knows their shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallHeader {
    pub xid: u32,
    pub prog: u32,
    pub vers: u32,
    pub proc: u32,
}

/// Why an incoming record could not be dispatched as a call. Each variant
/// maps to the reply the spec wants sent — or to none at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// Not parseable even as an RPC message header. There is no xid to
    /// reply to, so the connection is the only thing left to act on.
    Garbled(XdrError),
    /// A REPLY arrived where a call belongs. One-way interrupt delivery
    /// aside, a server receiving a reply has nothing to say back.
    NotACall { xid: u32 },
    /// rpcvers != 2 → deny with RPC_MISMATCH (needs the xid to do it).
    RpcVersion { xid: u32, vers: u32 },
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Garbled(e) => write!(f, "unparseable RPC message: {e}"),
            Self::NotACall { xid } => write!(f, "RPC reply (xid {xid:#x}) where a call belongs"),
            Self::RpcVersion { vers, .. } => write!(f, "RPC version {vers}, only 2 is spoken"),
        }
    }
}

impl std::error::Error for CallError {}

/// Decode one record as a call. Returns the header and the undecoded
/// procedure arguments.
pub fn decode_call(record: &[u8]) -> Result<(CallHeader, &[u8]), CallError> {
    let mut c = Cursor::new(record);
    let xid = c.u32().map_err(CallError::Garbled)?;
    let mtype = c.u32().map_err(CallError::Garbled)?;
    if mtype != CALL {
        return Err(CallError::NotACall { xid });
    }
    let rpcvers = c.u32().map_err(CallError::Garbled)?;
    if rpcvers != RPC_VERSION {
        return Err(CallError::RpcVersion { xid, vers: rpcvers });
    }
    let mut rest = || -> Result<CallHeader, XdrError> {
        let prog = c.u32()?;
        let vers = c.u32()?;
        let proc = c.u32()?;
        // cred and verf: flavor + opaque body, contents ignored (see module
        // comment). The lengths still have to parse — a truncated auth field
        // means we cannot find where the arguments start.
        for _ in 0..2 {
            let _flavor = c.u32()?;
            c.opaque(AUTH_BODY_MAX)?;
        }
        Ok(CallHeader {
            xid,
            prog,
            vers,
            proc,
        })
    };
    match rest() {
        Ok(header) => Ok((header, c.rest())),
        Err(e) => Err(CallError::Garbled(e)),
    }
}

/// The reply header shared by every accepted reply: xid, REPLY,
/// MSG_ACCEPTED, AUTH_NONE verifier, then the accept_stat.
fn accepted(xid: u32, stat: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    xdr::put_u32(&mut buf, xid);
    xdr::put_u32(&mut buf, REPLY);
    xdr::put_u32(&mut buf, MSG_ACCEPTED);
    xdr::put_u32(&mut buf, AUTH_NONE);
    xdr::put_opaque(&mut buf, &[]);
    xdr::put_u32(&mut buf, stat);
    buf
}

/// SUCCESS reply carrying procedure-specific results.
pub fn reply_success(xid: u32, results: &[u8]) -> Vec<u8> {
    let mut buf = accepted(xid, SUCCESS);
    buf.extend_from_slice(results);
    buf
}

pub fn reply_prog_unavail(xid: u32) -> Vec<u8> {
    accepted(xid, PROG_UNAVAIL)
}

/// PROG_MISMATCH carries the supported version range (RFC 5531 §9).
pub fn reply_prog_mismatch(xid: u32, low: u32, high: u32) -> Vec<u8> {
    let mut buf = accepted(xid, PROG_MISMATCH);
    xdr::put_u32(&mut buf, low);
    xdr::put_u32(&mut buf, high);
    buf
}

pub fn reply_proc_unavail(xid: u32) -> Vec<u8> {
    accepted(xid, PROC_UNAVAIL)
}

pub fn reply_garbage_args(xid: u32) -> Vec<u8> {
    accepted(xid, GARBAGE_ARGS)
}

pub fn reply_system_err(xid: u32) -> Vec<u8> {
    accepted(xid, SYSTEM_ERR)
}

/// MSG_DENIED / RPC_MISMATCH, with the version range we do speak.
pub fn reply_rpc_mismatch(xid: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(24);
    xdr::put_u32(&mut buf, xid);
    xdr::put_u32(&mut buf, REPLY);
    xdr::put_u32(&mut buf, MSG_DENIED);
    xdr::put_u32(&mut buf, RPC_MISMATCH);
    xdr::put_u32(&mut buf, RPC_VERSION);
    xdr::put_u32(&mut buf, RPC_VERSION);
    buf
}

/// Encode one call, AUTH_NONE cred and verf. The client half.
pub fn encode_call(xid: u32, prog: u32, vers: u32, proc: u32, args: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(40 + args.len());
    xdr::put_u32(&mut buf, xid);
    xdr::put_u32(&mut buf, CALL);
    xdr::put_u32(&mut buf, RPC_VERSION);
    xdr::put_u32(&mut buf, prog);
    xdr::put_u32(&mut buf, vers);
    xdr::put_u32(&mut buf, proc);
    for _ in 0..2 {
        xdr::put_u32(&mut buf, AUTH_NONE);
        xdr::put_opaque(&mut buf, &[]);
    }
    buf.extend_from_slice(args);
    buf
}

/// What a decoded reply said, from the client's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyBody<'a> {
    /// Procedure-specific results, undecoded.
    Success(&'a [u8]),
    /// Accepted but failed; the accept_stat says how.
    Accepted(u32),
    /// Denied outright (reject_stat).
    Denied(u32),
}

/// Why a record could not be understood as the awaited reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyError {
    Garbled(XdrError),
    /// A mismatched xid is an error rather than a skip: this client runs one
    /// call at a time per connection, so an unexpected xid is not an
    /// out-of-order answer to somebody else, it is a peer that lost framing.
    WrongXid {
        got: u32,
        expected: u32,
    },
    /// A CALL (or garbage discriminant) where a reply belongs.
    NotAReply(u32),
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Garbled(e) => write!(f, "unparseable RPC reply: {e}"),
            Self::WrongXid { got, expected } => {
                write!(f, "RPC reply xid {got:#x}, awaiting {expected:#x}")
            }
            Self::NotAReply(t) => write!(f, "RPC message type {t} where a reply belongs"),
        }
    }
}

impl std::error::Error for ReplyError {}

/// Decode one record as a reply to `expect_xid`. The client half.
pub fn decode_reply(record: &[u8], expect_xid: u32) -> Result<ReplyBody<'_>, ReplyError> {
    let mut c = Cursor::new(record);
    let xid = c.u32().map_err(ReplyError::Garbled)?;
    if xid != expect_xid {
        return Err(ReplyError::WrongXid {
            got: xid,
            expected: expect_xid,
        });
    }
    let mtype = c.u32().map_err(ReplyError::Garbled)?;
    if mtype != REPLY {
        return Err(ReplyError::NotAReply(mtype));
    }
    match c.u32().map_err(ReplyError::Garbled)? {
        MSG_ACCEPTED => {
            let _verf_flavor = c.u32().map_err(ReplyError::Garbled)?;
            c.opaque(AUTH_BODY_MAX).map_err(ReplyError::Garbled)?;
            match c.u32().map_err(ReplyError::Garbled)? {
                SUCCESS => Ok(ReplyBody::Success(c.rest())),
                stat => Ok(ReplyBody::Accepted(stat)),
            }
        }
        MSG_DENIED => Ok(ReplyBody::Denied(c.u32().map_err(ReplyError::Garbled)?)),
        other => Err(ReplyError::NotAReply(other)),
    }
}

/// Largest record accepted before the peer is judged broken or hostile.
/// Big enough for a full device_write against a 16 MB maxRecvSize story if
/// one is ever configured; the real per-link limit is enforced above this
/// layer, where it is known.
pub const RECORD_MAX: usize = 32 * 1024 * 1024;

/// Read one record (RFC 5531 §11): fragments of 4-byte header + data, the
/// header's top bit marking the last fragment. Returns `None` on clean EOF
/// at a record boundary — the peer hung up between calls, which is how every
/// RPC connection eventually ends and not an error.
pub async fn read_record<R>(rd: &mut R, max_len: usize) -> std::io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    use std::io::{Error, ErrorKind};
    let mut record = Vec::new();
    loop {
        let mut header = [0u8; 4];
        match rd.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof && record.is_empty() => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        let word = u32::from_be_bytes(header);
        let last = word & 0x8000_0000 != 0;
        let len = (word & 0x7FFF_FFFF) as usize;
        if record.len().saturating_add(len) > max_len {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("RPC record exceeds {max_len} bytes"),
            ));
        }
        let start = record.len();
        record.resize(start + len, 0);
        rd.read_exact(&mut record[start..]).await?;
        if last {
            return Ok(Some(record));
        }
    }
}

/// Write one record as a single last-fragment. Splitting outbound records
/// is legal but pointless — fragmentation exists so a sender need not know
/// a record's length up front, and ours is always in hand.
pub async fn write_record<W>(wr: &mut W, payload: &[u8]) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    use std::io::{Error, ErrorKind};
    if payload.len() > 0x7FFF_FFFF {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "RPC record too large for one fragment",
        ));
    }
    let header = 0x8000_0000u32 | payload.len() as u32;
    wr.write_all(&header.to_be_bytes()).await?;
    wr.write_all(payload).await?;
    wr.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// encode_call and decode_call are each other's proof.
    #[test]
    fn a_call_roundtrips_through_both_halves() {
        let record = encode_call(0x1234, 0x0607AF, 1, 10, b"args-bytes");
        let (header, args) = decode_call(&record).unwrap();
        assert_eq!(
            header,
            CallHeader {
                xid: 0x1234,
                prog: 0x0607AF,
                vers: 1,
                proc: 10,
            }
        );
        assert_eq!(args, b"args-bytes");
    }

    /// RFC 5531 §10: the credential is an opaque the server must skip even
    /// when the flavor is unknown to it. AUTH_SYS bodies are real-world
    /// common from VISA stacks.
    #[test]
    fn a_nonempty_credential_is_skipped_not_refused() {
        let mut record = Vec::new();
        xdr::put_u32(&mut record, 7);
        xdr::put_u32(&mut record, CALL);
        xdr::put_u32(&mut record, RPC_VERSION);
        xdr::put_u32(&mut record, 100000);
        xdr::put_u32(&mut record, 2);
        xdr::put_u32(&mut record, 3);
        xdr::put_u32(&mut record, 1); // AUTH_SYS
        xdr::put_opaque(&mut record, &[0xAA; 24]);
        xdr::put_u32(&mut record, 0); // verf AUTH_NONE
        xdr::put_opaque(&mut record, &[]);
        record.extend_from_slice(b"xyz!");
        let (header, args) = decode_call(&record).unwrap();
        assert_eq!(header.proc, 3);
        assert_eq!(args, b"xyz!");
    }

    #[test]
    fn an_oversized_credential_is_garbled_not_a_panic() {
        let mut record = Vec::new();
        xdr::put_u32(&mut record, 7);
        xdr::put_u32(&mut record, CALL);
        xdr::put_u32(&mut record, RPC_VERSION);
        xdr::put_u32(&mut record, 100000);
        xdr::put_u32(&mut record, 2);
        xdr::put_u32(&mut record, 3);
        xdr::put_u32(&mut record, 1);
        xdr::put_opaque(&mut record, &vec![0u8; 401]); // over RFC 5531 §8.2's 400
        assert!(matches!(
            decode_call(&record),
            Err(CallError::Garbled(XdrError::TooLong { len: 401, max: 400 }))
        ));
    }

    /// RFC 5531 §9: rpcvers != 2 is denied with RPC_MISMATCH, which needs
    /// the xid — so the decode must surface it rather than bail earlier.
    #[test]
    fn a_wrong_rpc_version_carries_its_xid_out_for_the_denial() {
        let mut record = Vec::new();
        xdr::put_u32(&mut record, 0xAB);
        xdr::put_u32(&mut record, CALL);
        xdr::put_u32(&mut record, 3);
        assert_eq!(
            decode_call(&record),
            Err(CallError::RpcVersion { xid: 0xAB, vers: 3 })
        );

        let denial = reply_rpc_mismatch(0xAB);
        let mut c = Cursor::new(&denial);
        assert_eq!(c.u32().unwrap(), 0xAB);
        assert_eq!(c.u32().unwrap(), REPLY);
        assert_eq!(c.u32().unwrap(), MSG_DENIED);
        assert_eq!(c.u32().unwrap(), RPC_MISMATCH);
        assert_eq!((c.u32().unwrap(), c.u32().unwrap()), (2, 2));
    }

    #[test]
    fn a_reply_where_a_call_belongs_is_not_dispatched() {
        let reply = reply_success(0x99, &[]);
        assert_eq!(decode_call(&reply), Err(CallError::NotACall { xid: 0x99 }));
    }

    #[test]
    fn success_replies_roundtrip_through_the_client_decoder() {
        let reply = reply_success(0x42, b"result!!");
        assert_eq!(
            decode_reply(&reply, 0x42).unwrap(),
            ReplyBody::Success(b"result!!")
        );
    }

    #[test]
    fn error_replies_surface_their_accept_stat() {
        for (encode, stat) in [
            (reply_prog_unavail as fn(u32) -> Vec<u8>, PROG_UNAVAIL),
            (reply_proc_unavail, PROC_UNAVAIL),
            (reply_garbage_args, GARBAGE_ARGS),
            (reply_system_err, SYSTEM_ERR),
        ] {
            assert_eq!(
                decode_reply(&encode(7), 7).unwrap(),
                ReplyBody::Accepted(stat)
            );
        }
    }

    #[test]
    fn prog_mismatch_reports_the_version_range() {
        let reply = reply_prog_mismatch(7, 1, 1);
        match decode_reply(&reply, 7).unwrap() {
            ReplyBody::Accepted(stat) => assert_eq!(stat, PROG_MISMATCH),
            other => panic!("expected accepted-with-error, got {other:?}"),
        }
    }

    #[test]
    fn a_mismatched_xid_is_a_framing_error() {
        let reply = reply_success(1, &[]);
        assert!(decode_reply(&reply, 2).is_err());
    }

    /// RFC 5531 §11: a record may arrive in any number of fragments; only
    /// the top bit of the last fragment's header says so.
    #[tokio::test]
    async fn a_record_split_across_fragments_reassembles() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&0x0000_0003u32.to_be_bytes()); // not last, 3 bytes
        wire.extend_from_slice(b"abc");
        wire.extend_from_slice(&0x0000_0000u32.to_be_bytes()); // empty middle fragment
        wire.extend_from_slice(&0x8000_0002u32.to_be_bytes()); // last, 2 bytes
        wire.extend_from_slice(b"de");
        let mut rd = wire.as_slice();
        let record = read_record(&mut rd, RECORD_MAX).await.unwrap().unwrap();
        assert_eq!(record, b"abcde");
    }

    #[tokio::test]
    async fn write_then_read_is_the_identity() {
        let mut wire = Vec::new();
        write_record(&mut wire, b"one record").await.unwrap();
        assert_eq!(wire[0], 0x80, "single last-fragment header");
        let mut rd = wire.as_slice();
        assert_eq!(
            read_record(&mut rd, RECORD_MAX).await.unwrap().unwrap(),
            b"one record"
        );
        assert!(read_record(&mut rd, RECORD_MAX).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn eof_at_a_record_boundary_is_a_hangup_not_an_error() {
        let mut rd: &[u8] = &[];
        assert!(read_record(&mut rd, RECORD_MAX).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn eof_inside_a_record_is_an_error() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&0x8000_0008u32.to_be_bytes());
        wire.extend_from_slice(b"shrt"); // promises 8, delivers 4
        let mut rd = wire.as_slice();
        assert!(read_record(&mut rd, RECORD_MAX).await.is_err());
    }

    /// The cumulative limit holds across fragments, or a peer could feed
    /// small fragments forever.
    #[tokio::test]
    async fn the_size_limit_is_cumulative_across_fragments() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&0x0000_0006u32.to_be_bytes());
        wire.extend_from_slice(b"sixsix");
        wire.extend_from_slice(&0x8000_0006u32.to_be_bytes());
        wire.extend_from_slice(b"sixsix");
        let mut rd = wire.as_slice();
        assert!(read_record(&mut rd, 8).await.is_err());
    }
}
