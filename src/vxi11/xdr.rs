// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// XDR (RFC 4506) — only the pieces ONC-RPC and VXI-11 actually use.
//
// Everything is big-endian and padded to the 4-byte basic block size
// (RFC 4506 §3). Decoding is strict: short input, an over-limit length, or
// nonzero padding where zeros belong are errors, not best-effort values — a
// codec that guesses hands the server a plausible lie about what the client
// said.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XdrError {
    /// Input ended before the field it should contain.
    Truncated,
    /// A variable-length item claims more bytes than its protocol limit.
    /// Carrying the limit makes the refusal auditable against the spec.
    TooLong { len: u32, max: u32 },
    /// A bool was neither 0 nor 1 (RFC 4506 §4.4 defines exactly those).
    BadBool(u32),
}

impl fmt::Display for XdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "XDR data truncated"),
            Self::TooLong { len, max } => {
                write!(
                    f,
                    "XDR variable-length item of {len} bytes exceeds limit {max}"
                )
            }
            Self::BadBool(v) => write!(f, "XDR bool encoded as {v}, must be 0 or 1"),
        }
    }
}

impl std::error::Error for XdrError {}

/// Bytes of zero padding that follow `len` bytes of data (RFC 4506 §3).
pub fn pad_of(len: usize) -> usize {
    (4 - len % 4) % 4
}

pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub fn put_i32(buf: &mut Vec<u8>, v: i32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub fn put_bool(buf: &mut Vec<u8>, v: bool) {
    put_u32(buf, v as u32);
}

/// Variable-length opaque (RFC 4506 §4.10): length, data, zero padding.
pub fn put_opaque(buf: &mut Vec<u8>, data: &[u8]) {
    put_u32(buf, data.len() as u32);
    buf.extend_from_slice(data);
    buf.extend(std::iter::repeat(0u8).take(pad_of(data.len())));
}

/// A string is encoded exactly like variable-length opaque (RFC 4506 §4.11).
/// VXI-11 device names and lock strings are ASCII, so no UTF-8 handling here;
/// what arrives is handed on as bytes and interpreted by the caller.
pub fn put_string(buf: &mut Vec<u8>, s: &str) {
    put_opaque(buf, s.as_bytes());
}

/// Strict reader over one XDR-encoded buffer.
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], XdrError> {
        let end = self.pos.checked_add(n).ok_or(XdrError::Truncated)?;
        if end > self.data.len() {
            return Err(XdrError::Truncated);
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn u32(&mut self) -> Result<u32, XdrError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32, XdrError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn bool(&mut self) -> Result<bool, XdrError> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(XdrError::BadBool(v)),
        }
    }

    /// Variable-length opaque, refused above `max`. The padding is consumed
    /// but deliberately not required to be zero: RFC 4506 §4.10 says senders
    /// MUST pad with zeros, but real client libraries have shipped garbage
    /// padding for decades and interoperating with them matters more than
    /// policing a field nothing interprets.
    pub fn opaque(&mut self, max: u32) -> Result<&'a [u8], XdrError> {
        let len = self.u32()?;
        if len > max {
            return Err(XdrError::TooLong { len, max });
        }
        let data = self.take(len as usize)?;
        self.take(pad_of(len as usize))?;
        Ok(data)
    }

    /// How many bytes remain unread. The record-level "did the caller send
    /// exactly one request" check belongs to whoever owns the record, since
    /// only they know whether trailing data is the next field or an error.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Everything not yet consumed, in one step. For the procedure-specific
    /// arguments that follow an RPC call header.
    pub fn rest(&mut self) -> &'a [u8] {
        let slice = &self.data[self.pos..];
        self.pos = self.data.len();
        slice
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u32_and_i32_are_big_endian() {
        let mut buf = Vec::new();
        put_u32(&mut buf, 0x0607AF); // DEVICE_CORE, as it happens
        put_i32(&mut buf, -2);
        assert_eq!(buf, [0x00, 0x06, 0x07, 0xAF, 0xFF, 0xFF, 0xFF, 0xFE]);
        let mut c = Cursor::new(&buf);
        assert_eq!(c.u32().unwrap(), 0x0607AF);
        assert_eq!(c.i32().unwrap(), -2);
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn opaque_pads_to_the_four_byte_block() {
        // RFC 4506 §4.10: r bytes of data, (4 - (r mod 4)) mod 4 residual zeros.
        for (len, padded) in [(0usize, 0usize), (1, 4), (3, 4), (4, 4), (5, 8)] {
            let data = vec![0xAB; len];
            let mut buf = Vec::new();
            put_opaque(&mut buf, &data);
            assert_eq!(buf.len(), 4 + padded, "data length {len}");
            let mut c = Cursor::new(&buf);
            assert_eq!(c.opaque(u32::MAX).unwrap(), &data[..]);
            assert_eq!(c.remaining(), 0, "padding consumed for length {len}");
        }
    }

    #[test]
    fn opaque_over_the_limit_is_refused_with_both_numbers() {
        let mut buf = Vec::new();
        put_opaque(&mut buf, &[0u8; 8]);
        let mut c = Cursor::new(&buf);
        assert_eq!(c.opaque(7), Err(XdrError::TooLong { len: 8, max: 7 }));
    }

    #[test]
    fn truncation_is_an_error_not_a_short_value() {
        let mut buf = Vec::new();
        put_u32(&mut buf, 12); // claims 12 bytes of opaque, provides none
        let mut c = Cursor::new(&buf);
        assert_eq!(c.opaque(u32::MAX), Err(XdrError::Truncated));

        let mut c = Cursor::new(&[0x00, 0x01]);
        assert_eq!(c.u32(), Err(XdrError::Truncated));
    }

    #[test]
    fn a_bool_is_zero_or_one_and_nothing_else() {
        let mut buf = Vec::new();
        put_bool(&mut buf, true);
        put_bool(&mut buf, false);
        put_u32(&mut buf, 2);
        let mut c = Cursor::new(&buf);
        assert!(c.bool().unwrap());
        assert!(!c.bool().unwrap());
        assert_eq!(c.bool(), Err(XdrError::BadBool(2)));
    }

    #[test]
    fn string_is_opaque_with_the_same_padding() {
        let mut buf = Vec::new();
        put_string(&mut buf, "gpib0,18");
        // 4 length + 8 data, already aligned.
        assert_eq!(buf.len(), 12);
        let mut c = Cursor::new(&buf);
        assert_eq!(c.opaque(1024).unwrap(), b"gpib0,18");
    }

    #[test]
    fn rest_hands_over_the_procedure_arguments_untouched() {
        let mut buf = Vec::new();
        put_u32(&mut buf, 7);
        buf.extend_from_slice(b"payload");
        let mut c = Cursor::new(&buf);
        c.u32().unwrap();
        assert_eq!(c.rest(), b"payload");
        assert_eq!(c.remaining(), 0);
    }
}
