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
