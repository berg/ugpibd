// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// The VXI-11 RPCL structures, encoded per the spec's own field order —
// which is worth spelling out, because it is inconsistent on purpose-unknown
// grounds and mis-ordering two u32 fields produces garbage that still
// parses: Device_WriteParms goes (io_timeout, lock_timeout, flags) while
// Device_GenericParms goes (flags, lock_timeout, io_timeout) and
// Device_LockParms (flags, lock_timeout). The golden-bytes tests below pin
// each order against hand-encoded XDR so a transposition cannot survive.
//
// Each struct carries both directions (server decodes parms + encodes
// resps; the client half encodes parms + decodes resps) because the CLI,
// the tests, and the interrupt channel — where the daemon is the RPC client
// — need the mirror image of what the server needs.

use super::xdr::{self, Cursor, XdrError};

/// The spec bounds an enable_srq handle at 40 bytes (`opaque handle<40>`).
pub const SRQ_HANDLE_MAX: u32 = 40;

/// The spec leaves the create_link device name unbounded (`string device<>`);
/// this cap is ours, far above any real name ("gpib0,30,96" is the shape),
/// so a garbage length cannot ask for gigabytes.
pub const DEVICE_NAME_MAX: u32 = 1024;

/// Create_LinkParms
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateLinkParms {
    pub client_id: i32,
    pub lock_device: bool,
    pub lock_timeout_ms: u32,
    pub device: Vec<u8>,
}

impl CreateLinkParms {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        xdr::put_i32(buf, self.client_id);
        xdr::put_bool(buf, self.lock_device);
        xdr::put_u32(buf, self.lock_timeout_ms);
        xdr::put_opaque(buf, &self.device);
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            client_id: c.i32()?,
            lock_device: c.bool()?,
            lock_timeout_ms: c.u32()?,
            device: c.opaque(DEVICE_NAME_MAX)?.to_vec(),
        })
    }
}

/// Create_LinkResp. `abort_port` is `unsigned short` in the spec, which XDR
/// widens to a full word on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateLinkResp {
    pub error: u32,
    pub lid: i32,
    pub abort_port: u16,
    pub max_recv_size: u32,
}

impl CreateLinkResp {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16);
        xdr::put_u32(&mut buf, self.error);
        xdr::put_i32(&mut buf, self.lid);
        xdr::put_u32(&mut buf, self.abort_port as u32);
        xdr::put_u32(&mut buf, self.max_recv_size);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            error: c.u32()?,
            lid: c.i32()?,
            abort_port: c.u32()? as u16,
            max_recv_size: c.u32()?,
        })
    }
}

/// Device_WriteParms — field order (io_timeout, lock_timeout, flags).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceWriteParms {
    pub lid: i32,
    pub io_timeout_ms: u32,
    pub lock_timeout_ms: u32,
    pub flags: u32,
    pub data: Vec<u8>,
}

impl DeviceWriteParms {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        xdr::put_i32(buf, self.lid);
        xdr::put_u32(buf, self.io_timeout_ms);
        xdr::put_u32(buf, self.lock_timeout_ms);
        xdr::put_u32(buf, self.flags);
        xdr::put_opaque(buf, &self.data);
    }

    /// `max_data` is the link's negotiated maxRecvSize: a write larger than
    /// what create_link promised is the client's protocol error, surfaced by
    /// the caller as ParameterError rather than decoded here into a buffer
    /// the server said it would not accept.
    pub fn decode(data: &[u8], max_data: u32) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            lid: c.i32()?,
            io_timeout_ms: c.u32()?,
            lock_timeout_ms: c.u32()?,
            flags: c.u32()?,
            data: c.opaque(max_data)?.to_vec(),
        })
    }
}

/// Device_WriteResp
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceWriteResp {
    pub error: u32,
    pub size: u32,
}

impl DeviceWriteResp {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        xdr::put_u32(&mut buf, self.error);
        xdr::put_u32(&mut buf, self.size);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            error: c.u32()?,
            size: c.u32()?,
        })
    }
}

/// Device_ReadParms — field order (request_size, io_timeout, lock_timeout,
/// flags, term_char).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceReadParms {
    pub lid: i32,
    pub request_size: u32,
    pub io_timeout_ms: u32,
    pub lock_timeout_ms: u32,
    pub flags: u32,
    /// Meaningful only when `flags` has OP_FLAG_TERMCHRSET. The RPCL spells
    /// it `char`, but XDR has no sub-word integer: it rides as a full word,
    /// low byte the character (pyvisa-py packs it as an int, identically).
    pub term_char: i32,
}

impl DeviceReadParms {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        xdr::put_i32(buf, self.lid);
        xdr::put_u32(buf, self.request_size);
        xdr::put_u32(buf, self.io_timeout_ms);
        xdr::put_u32(buf, self.lock_timeout_ms);
        xdr::put_u32(buf, self.flags);
        xdr::put_i32(buf, self.term_char);
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            lid: c.i32()?,
            request_size: c.u32()?,
            io_timeout_ms: c.u32()?,
            lock_timeout_ms: c.u32()?,
            flags: c.u32()?,
            term_char: c.i32()?,
        })
    }
}

/// Device_ReadResp
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceReadResp {
    pub error: u32,
    /// RX_REQCNT / RX_CHR / RX_END bits; 0 means the read ended for none of
    /// those reasons (an error, typically a timeout).
    pub reason: u32,
    pub data: Vec<u8>,
}

impl DeviceReadResp {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12 + self.data.len());
        xdr::put_u32(&mut buf, self.error);
        xdr::put_u32(&mut buf, self.reason);
        xdr::put_opaque(&mut buf, &self.data);
        buf
    }

    pub fn decode(data: &[u8], max_data: u32) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            error: c.u32()?,
            reason: c.u32()?,
            data: c.opaque(max_data)?.to_vec(),
        })
    }
}

/// Device_GenericParms — field order (flags, lock_timeout, io_timeout),
/// the reverse of Device_WriteParms' timeouts. Used by readstb, trigger,
/// clear, remote, and local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceGenericParms {
    pub lid: i32,
    pub flags: u32,
    pub lock_timeout_ms: u32,
    pub io_timeout_ms: u32,
}

impl DeviceGenericParms {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        xdr::put_i32(buf, self.lid);
        xdr::put_u32(buf, self.flags);
        xdr::put_u32(buf, self.lock_timeout_ms);
        xdr::put_u32(buf, self.io_timeout_ms);
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            lid: c.i32()?,
            flags: c.u32()?,
            lock_timeout_ms: c.u32()?,
            io_timeout_ms: c.u32()?,
        })
    }
}

/// Device_ReadStbResp
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceReadStbResp {
    pub error: u32,
    pub stb: u8,
}

impl DeviceReadStbResp {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        xdr::put_u32(&mut buf, self.error);
        xdr::put_u32(&mut buf, self.stb as u32);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            error: c.u32()?,
            stb: c.u32()? as u8,
        })
    }
}

/// Device_LockParms — field order (flags, lock_timeout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceLockParms {
    pub lid: i32,
    pub flags: u32,
    pub lock_timeout_ms: u32,
}

impl DeviceLockParms {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        xdr::put_i32(buf, self.lid);
        xdr::put_u32(buf, self.flags);
        xdr::put_u32(buf, self.lock_timeout_ms);
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            lid: c.i32()?,
            flags: c.u32()?,
            lock_timeout_ms: c.u32()?,
        })
    }
}

/// Device_EnableSrqParms
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEnableSrqParms {
    pub lid: i32,
    pub enable: bool,
    pub handle: Vec<u8>,
}

impl DeviceEnableSrqParms {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        xdr::put_i32(buf, self.lid);
        xdr::put_bool(buf, self.enable);
        xdr::put_opaque(buf, &self.handle);
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            lid: c.i32()?,
            enable: c.bool()?,
            handle: c.opaque(SRQ_HANDLE_MAX)?.to_vec(),
        })
    }
}

/// Device_RemoteFunc — the client's interrupt-channel endpoint, delivered
/// via create_intr_chan. `prog_family` takes DEVICE_TCP / DEVICE_UDP, the
/// spec's own enum — not IPPROTO numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRemoteFunc {
    /// IPv4 address as a big-endian integer, as the spec predates anything
    /// else.
    pub host_addr: u32,
    pub host_port: u16,
    pub prog_num: u32,
    pub prog_vers: u32,
    pub prog_family: u32,
}

impl DeviceRemoteFunc {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        xdr::put_u32(buf, self.host_addr);
        xdr::put_u32(buf, self.host_port as u32);
        xdr::put_u32(buf, self.prog_num);
        xdr::put_u32(buf, self.prog_vers);
        xdr::put_u32(buf, self.prog_family);
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            host_addr: c.u32()?,
            host_port: c.u32()? as u16,
            prog_num: c.u32()?,
            prog_vers: c.u32()?,
            prog_family: c.u32()?,
        })
    }
}

/// Device_SrqParms — the one argument of device_intr_srq, echoing the
/// handle device_enable_srq registered, byte for byte (RULE B.6.111).
///
/// The RPCL leaves this handle unbounded (`opaque handle<>`, §C.2) where the
/// enable side says `handle<40>` — but B.6.111 makes the sent bytes a copy
/// of the registered ones, so the 40-byte cap in `decode` can only refuse
/// traffic a conforming server would never emit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSrqParms {
    pub handle: Vec<u8>,
}

impl DeviceSrqParms {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.handle.len());
        xdr::put_opaque(&mut buf, &self.handle);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            handle: c.opaque(SRQ_HANDLE_MAX)?.to_vec(),
        })
    }
}

/// Device_DocmdParms
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDocmdParms {
    pub lid: i32,
    pub flags: u32,
    pub io_timeout_ms: u32,
    pub lock_timeout_ms: u32,
    pub cmd: i32,
    pub network_order: bool,
    pub datasize: i32,
    pub data_in: Vec<u8>,
}

impl DeviceDocmdParms {
    /// docmd payloads are interface commands, small by nature.
    const DATA_MAX: u32 = 4096;

    pub fn encode(&self, buf: &mut Vec<u8>) {
        xdr::put_i32(buf, self.lid);
        xdr::put_u32(buf, self.flags);
        xdr::put_u32(buf, self.io_timeout_ms);
        xdr::put_u32(buf, self.lock_timeout_ms);
        xdr::put_i32(buf, self.cmd);
        xdr::put_bool(buf, self.network_order);
        xdr::put_i32(buf, self.datasize);
        xdr::put_opaque(buf, &self.data_in);
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            lid: c.i32()?,
            flags: c.u32()?,
            io_timeout_ms: c.u32()?,
            lock_timeout_ms: c.u32()?,
            cmd: c.i32()?,
            network_order: c.bool()?,
            datasize: c.i32()?,
            data_in: c.opaque(Self::DATA_MAX)?.to_vec(),
        })
    }
}

/// Device_DocmdResp
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDocmdResp {
    pub error: u32,
    pub data_out: Vec<u8>,
}

impl DeviceDocmdResp {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.data_out.len());
        xdr::put_u32(&mut buf, self.error);
        xdr::put_opaque(&mut buf, &self.data_out);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(data);
        Ok(Self {
            error: c.u32()?,
            data_out: c.opaque(DeviceDocmdParms::DATA_MAX)?.to_vec(),
        })
    }
}

/// Device_Error — the whole response of trigger, clear, remote, local,
/// unlock, enable_srq, destroy_link, and the intr_chan pair.
pub fn encode_device_error(error: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4);
    xdr::put_u32(&mut buf, error);
    buf
}

pub fn decode_device_error(data: &[u8]) -> Result<u32, XdrError> {
    Cursor::new(data).u32()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field order pinned against hand-encoded XDR, because a transposed
    /// pair of u32 fields still parses. Layout cross-checked against
    /// pyvisa-py's Vxi11Packer, the interop target's own transcription of
    /// the spec's RPCL.
    #[test]
    fn create_link_parms_golden_bytes() {
        let parms = CreateLinkParms {
            client_id: 0x0102_0304,
            lock_device: true,
            lock_timeout_ms: 0x0A0B_0C0D,
            device: b"gpib0,18".to_vec(),
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        #[rustfmt::skip]
        let golden: &[u8] = &[
            0x01, 0x02, 0x03, 0x04,
            0x00, 0x00, 0x00, 0x01,
            0x0A, 0x0B, 0x0C, 0x0D,
            0x00, 0x00, 0x00, 0x08, b'g', b'p', b'i', b'b', b'0', b',', b'1', b'8',
        ];
        assert_eq!(buf, golden);
        assert_eq!(CreateLinkParms::decode(&buf).unwrap(), parms);
    }

    /// write goes (io_timeout, lock_timeout, flags)…
    #[test]
    fn device_write_parms_timeout_order_golden_bytes() {
        let parms = DeviceWriteParms {
            lid: 1,
            io_timeout_ms: 0x1111_1111,
            lock_timeout_ms: 0x2222_2222,
            flags: 0x0000_0008,
            data: b"*IDN?\n".to_vec(),
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        #[rustfmt::skip]
        let golden: &[u8] = &[
            0x00, 0x00, 0x00, 0x01,
            0x11, 0x11, 0x11, 0x11, // io_timeout FIRST
            0x22, 0x22, 0x22, 0x22, // lock_timeout second
            0x00, 0x00, 0x00, 0x08, // flags third
            0x00, 0x00, 0x00, 0x06, b'*', b'I', b'D', b'N', b'?', b'\n', 0x00, 0x00,
        ];
        assert_eq!(buf, golden);
        assert_eq!(DeviceWriteParms::decode(&buf, 1024).unwrap(), parms);
    }

    /// …while the generic parms reverse it: (flags, lock_timeout, io_timeout).
    #[test]
    fn device_generic_parms_reversed_order_golden_bytes() {
        let parms = DeviceGenericParms {
            lid: 1,
            flags: 0x0000_0001,
            lock_timeout_ms: 0x2222_2222,
            io_timeout_ms: 0x1111_1111,
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        #[rustfmt::skip]
        let golden: &[u8] = &[
            0x00, 0x00, 0x00, 0x01,
            0x00, 0x00, 0x00, 0x01, // flags FIRST
            0x22, 0x22, 0x22, 0x22, // lock_timeout second
            0x11, 0x11, 0x11, 0x11, // io_timeout LAST
        ];
        assert_eq!(buf, golden);
        assert_eq!(DeviceGenericParms::decode(&buf).unwrap(), parms);
    }

    #[test]
    fn read_parms_roundtrip_with_termchar() {
        let parms = DeviceReadParms {
            lid: 3,
            request_size: 20480,
            io_timeout_ms: 25000,
            lock_timeout_ms: 0,
            flags: super::super::OP_FLAG_TERMCHRSET,
            term_char: b'\n' as i32,
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        assert_eq!(buf.len(), 24);
        assert_eq!(DeviceReadParms::decode(&buf).unwrap(), parms);
    }

    #[test]
    fn every_response_roundtrips() {
        let resp = CreateLinkResp {
            error: 0,
            lid: 42,
            abort_port: 9011,
            max_recv_size: 65536,
        };
        assert_eq!(CreateLinkResp::decode(&resp.encode()).unwrap(), resp);

        let resp = DeviceWriteResp { error: 0, size: 6 };
        assert_eq!(DeviceWriteResp::decode(&resp.encode()).unwrap(), resp);

        let resp = DeviceReadResp {
            error: 0,
            reason: super::super::RX_END,
            data: b"HP8594E\r\n".to_vec(),
        };
        assert_eq!(DeviceReadResp::decode(&resp.encode(), 1024).unwrap(), resp);

        let resp = DeviceReadStbResp {
            error: 0,
            stb: 0x50,
        };
        assert_eq!(DeviceReadStbResp::decode(&resp.encode()).unwrap(), resp);

        let resp = DeviceDocmdResp {
            error: 0,
            data_out: vec![0x21],
        };
        assert_eq!(DeviceDocmdResp::decode(&resp.encode()).unwrap(), resp);

        assert_eq!(decode_device_error(&encode_device_error(15)).unwrap(), 15);
    }

    #[test]
    fn lock_enable_srq_remote_func_and_docmd_roundtrip() {
        let parms = DeviceLockParms {
            lid: 7,
            flags: super::super::OP_FLAG_WAITLOCK,
            lock_timeout_ms: 3000,
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        assert_eq!(DeviceLockParms::decode(&buf).unwrap(), parms);

        let parms = DeviceEnableSrqParms {
            lid: 7,
            enable: true,
            handle: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        assert_eq!(DeviceEnableSrqParms::decode(&buf).unwrap(), parms);

        let parms = DeviceRemoteFunc {
            host_addr: u32::from_be_bytes([127, 0, 0, 1]),
            host_port: 51234,
            prog_num: super::super::DEVICE_INTR_PROG,
            prog_vers: super::super::DEVICE_INTR_VERS,
            prog_family: super::super::DEVICE_TCP,
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        assert_eq!(DeviceRemoteFunc::decode(&buf).unwrap(), parms);

        let parms = DeviceDocmdParms {
            lid: 1,
            flags: 0,
            io_timeout_ms: 1000,
            lock_timeout_ms: 0,
            cmd: 0x020001,
            network_order: true,
            datasize: 2,
            data_in: vec![0, 1],
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        assert_eq!(DeviceDocmdParms::decode(&buf).unwrap(), parms);
    }

    #[test]
    fn an_srq_handle_over_forty_bytes_is_refused() {
        // opaque handle<40> in the spec's RPCL.
        let parms = DeviceEnableSrqParms {
            lid: 1,
            enable: true,
            handle: vec![0; 41],
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        assert_eq!(
            DeviceEnableSrqParms::decode(&buf),
            Err(XdrError::TooLong { len: 41, max: 40 })
        );
    }

    #[test]
    fn a_write_larger_than_the_negotiated_max_is_refused_at_decode() {
        let parms = DeviceWriteParms {
            lid: 1,
            io_timeout_ms: 0,
            lock_timeout_ms: 0,
            flags: 0,
            data: vec![0; 100],
        };
        let mut buf = Vec::new();
        parms.encode(&mut buf);
        assert_eq!(
            DeviceWriteParms::decode(&buf, 64),
            Err(XdrError::TooLong { len: 100, max: 64 })
        );
    }
}
