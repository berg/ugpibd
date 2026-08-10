// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// VXI-11 (TCP/IP Instrument Protocol) server.
//
// VXI-11 is the front-end whose wire protocol matches GPIB semantics:
// `device_read` is an explicit RPC, so the daemon addresses the instrument
// to talk because the client asked it to — no read-after-write guessing, no
// status-bit heuristics. It is what the classic LAN/GPIB gateways
// (HP/Agilent E2050, E5810) speak. See docs/VXI11-PLAN.md for the build
// plan and spec references.
//
// Constants below are from the VXI-11 specification (VXIbus Consortium,
// rev 1.0) RPCL definitions, cross-checked against pyvisa-py's
// `protocols/vxi11.py`, which is the interop target's own transcription.

pub mod client;
pub mod messages;
pub mod portmap;
pub mod rpc;
pub mod server;
pub mod xdr;

/// Core channel: the RPC program the client calls us on (TCP).
pub const DEVICE_CORE_PROG: u32 = 0x0607AF;
pub const DEVICE_CORE_VERS: u32 = 1;

/// Abort channel: a second server-side program on its own port, so an abort
/// can overtake the in-flight core call it names.
pub const DEVICE_ASYNC_PROG: u32 = 0x0607B0;
pub const DEVICE_ASYNC_VERS: u32 = 1;

/// Interrupt channel: served by the *client*; the daemon calls it back to
/// deliver service requests.
pub const DEVICE_INTR_PROG: u32 = 0x0607B1;
pub const DEVICE_INTR_VERS: u32 = 1;

/// DEVICE_CORE procedures.
pub const CREATE_LINK: u32 = 10;
pub const DEVICE_WRITE: u32 = 11;
pub const DEVICE_READ: u32 = 12;
pub const DEVICE_READSTB: u32 = 13;
pub const DEVICE_TRIGGER: u32 = 14;
pub const DEVICE_CLEAR: u32 = 15;
pub const DEVICE_REMOTE: u32 = 16;
pub const DEVICE_LOCAL: u32 = 17;
pub const DEVICE_LOCK: u32 = 18;
pub const DEVICE_UNLOCK: u32 = 19;
pub const DEVICE_ENABLE_SRQ: u32 = 20;
pub const DEVICE_DOCMD: u32 = 22;
pub const DESTROY_LINK: u32 = 23;
pub const CREATE_INTR_CHAN: u32 = 25;
pub const DESTROY_INTR_CHAN: u32 = 26;

/// DEVICE_ASYNC procedure.
pub const DEVICE_ABORT: u32 = 1;

/// DEVICE_INTR procedure.
pub const DEVICE_INTR_SRQ: u32 = 30;

/// `Device_ErrorCode`. The gaps are the spec's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    NoError = 0,
    SyntaxError = 1,
    DeviceNotAccessible = 3,
    InvalidLinkIdentifier = 4,
    ParameterError = 5,
    ChannelNotEstablished = 6,
    OperationNotSupported = 8,
    OutOfResources = 9,
    DeviceLockedByAnotherLink = 11,
    NoLockHeldByThisLink = 12,
    IoTimeout = 15,
    IoError = 17,
    /// In the spec's error tables (B.4) though absent from some client
    /// transcriptions: refuses addresses that parse but cannot exist here
    /// (PAD > 30, secondary addressing).
    InvalidAddress = 21,
    Abort = 23,
    ChannelAlreadyEstablished = 29,
}

impl ErrorCode {
    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

/// `Device_Flags` bits.
pub const OP_FLAG_WAITLOCK: u32 = 1;
/// device_write: assert EOI with the last byte.
pub const OP_FLAG_END: u32 = 8;
/// device_read: the termChar field is meaningful.
pub const OP_FLAG_TERMCHRSET: u32 = 128;

/// device_read termination reasons.
pub const RX_REQCNT: u32 = 1;
pub const RX_CHR: u32 = 2;
pub const RX_END: u32 = 4;

/// create_intr_chan address family values. These are the spec's own
/// `Device_AddrFamily` enum — NOT the IPPROTO numbers portmap uses, an easy
/// and observed confusion.
pub const DEVICE_TCP: u32 = 0;
pub const DEVICE_UDP: u32 = 1;
