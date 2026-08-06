// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("device returned error code {code:#x} for command {cmd:#x}")]
    DeviceError { cmd: u8, code: u8 },
    #[error("unexpected response byte {got:#x}, expected {expected:#x}")]
    BadResponse { expected: u8, got: u8 },
    #[error("response too short: got {got} bytes, need at least {need}")]
    ShortResponse { need: usize, got: usize },
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkCmd {
    Write = 0x01,
    Read = 0x03,
    WrRegs = 0x04,
    RdRegs = 0x05,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub enum WriteFlag {
    SendEoi = 0x01,
    NoFastTalkerFirstByte = 0x02,
    NoFastTalker = 0x04,
    NoAddress = 0x08,
    Atn = 0x10,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub enum ReadFlag {
    EndOnEoi = 0x01,
    NoAddress = 0x02,
    EndOnEosChar = 0x04,
}

pub const ATRF_EOI: u8 = 0x01;
pub const ATRF_ATN: u8 = 0x02;
pub const ATRF_IFC: u8 = 0x04;
pub const ATRF_EOS: u8 = 0x08;
pub const ATRF_ABORT: u8 = 0x10;
pub const ATRF_COUNT: u8 = 0x20;
pub const ATRF_DEAD_BUS: u8 = 0x40;
/// The adapter refused the read outright: it did not consider itself a
/// listener, never armed the transfer, and answered with this flag alone.
/// See `docs/CAPTURE.md` §14.18 — a read that ends this way returns
/// *immediately*, which is the observable difference between "refused" and
/// "armed but the bus stayed quiet" (a timeout).
pub const ATRF_UNADDRESSED: u8 = 0x80;

pub const XFER_ABORT: u16 = 0xa0;
pub const XFER_STATUS: u16 = 0xb0;
pub const CONTROL_REQUEST: u8 = 0x04;

pub const UGP_SUCCESS: u8 = 0x00;
pub const UGP_ERR_FLUSHING: u8 = 0x06;
pub const UGP_ERR_FLUSHING_ALREADY: u8 = 0x07;

pub const XA_FLUSH: u16 = 0x01;

// Firmware-level register addresses (not TMS9914)
pub const REG_HW_CONTROL: u8 = 0x0a;
pub const REG_LED_CONTROL: u8 = 0x0b;
pub const REG_RESET_TO_POWERUP: u8 = 0x0c;
pub const REG_PROTOCOL_CONTROL: u8 = 0x0d;
pub const REG_FAST_TALKER_T1: u8 = 0x0e;

// Hardware control bits
pub const NOT_TI_RESET: u8 = 0x01;
pub const SYSTEM_CONTROLLER: u8 = 0x02;
pub const NOT_PARALLEL_POLL: u8 = 0x04;

// LED control bits
pub const FIRMWARE_LED_CONTROL: u8 = 0x01;
pub const FAIL_LED_ON: u8 = 0x20;

// Protocol control bits
pub const WRITE_COMPLETE_INTERRUPT_EN: u8 = 0x01;

// Reset bits
pub const RESET_SPACEBALL: u8 = 0x01;

// TMS9914 *read* register addresses. These are a different register space from
// the write addresses below: offset 3 is AUXCR when written and the bus status
// register when read. Reading ISR0/ISR1 (0 and 1) clears pending interrupt
// status and would steal notifications from the interrupt endpoint, so only the
// side-effect-free bus status register is exposed here.
pub const TMS_BSR: u8 = 0x03;

// Bus status register bits — a live read of the physical GPIB lines. Bit order
// established on hardware rather than from a datasheet: an idle bus reads 0x29
// and the same bus with an instrument asserting SRQ reads 0x2d, so SRQ is 0x04.
// REN corroborates it — the daemon asserts REN at init and never drops it, and
// bit 0x01 is set in every reading.
//
//   bit 7 ATN   bit 6 DAV   bit 5 NDAC  bit 4 NRFD
//   bit 3 EOI   bit 2 SRQ   bit 1 IFC   bit 0 REN
//
// Watch out for NDAC (0x20): idle listeners hold it asserted, so mistaking it
// for SRQ reads as "someone is always requesting service".
//
// The empirical order above is corroborated by the kernel's own header —
// `tms9914.h:236-243` defines BSR_REN_BIT..BSR_ATN_BIT as 0x01..0x80 in this
// same sequence — and by the TNT4882, whose bus status register uses an
// identical layout (`ni_usb_hs/protocol.rs:60-67`).
pub const BSR_REN: u8 = 0x01;
pub const BSR_IFC: u8 = 0x02;
pub const BSR_SRQ: u8 = 0x04;
pub const BSR_EOI: u8 = 0x08;
pub const BSR_NRFD: u8 = 0x10;
pub const BSR_NDAC: u8 = 0x20;
pub const BSR_DAV: u8 = 0x40;
pub const BSR_ATN: u8 = 0x80;

// TMS9914 write register addresses
pub const TMS_IMR0: u8 = 0x00;
pub const TMS_IMR1: u8 = 0x01;
pub const TMS_AUXCR: u8 = 0x03;
pub const TMS_ADR: u8 = 0x04;
pub const TMS_SPMR: u8 = 0x05;
pub const TMS_PPR: u8 = 0x06;

// TMS9914 AUXCR command values
pub const AUX_CS: u8 = 0x80;
pub const AUX_CHIP_RESET: u8 = 0x00;
pub const AUX_INVAL: u8 = 0x01;
pub const AUX_HLDE: u8 = 0x04;
pub const AUX_NBAF: u8 = 0x05;
pub const AUX_LON: u8 = 0x09;
/// Release a DAC holdoff, valid command byte (`AUX_INVAL | AUX_CS`).
pub const AUX_VAL: u8 = 0x81;
pub const AUX_TON: u8 = 0x0a;
pub const AUX_GTS: u8 = 0x0b;
pub const AUX_TCA: u8 = 0x0c;
pub const AUX_SIC: u8 = 0x0f;
pub const AUX_SRE: u8 = 0x10;
pub const AUX_RQC: u8 = 0x11;
pub const AUX_STDL: u8 = 0x15;
pub const AUX_VSTDL: u8 = 0x17;
pub const AUX_RSV2: u8 = 0x18;
pub const AUX_RPP: u8 = 0x0e;

// TMS9914 interrupt mask bits
pub const HR_BOIE: u8 = 0x10;
pub const HR_BIIE: u8 = 0x20;
pub const HR_SRQIE: u8 = 0x02;

// ADR register
pub const ADDRESS_MASK: u8 = 0x1f;

// IEEE-488 bus command bytes, sent with ATN asserted.
pub const GPIB_UNL: u8 = 0x3f;
pub const GPIB_UNT: u8 = 0x5f;
pub const GPIB_GTL: u8 = 0x01;
pub const GPIB_LLO: u8 = 0x11;
pub const GPIB_SDC: u8 = 0x04;
pub const GPIB_GET: u8 = 0x08;
pub const GPIB_SPE: u8 = 0x18;
pub const GPIB_SPD: u8 = 0x19;

/// Listen address for primary address `pad` (our own pad gives MLA).
pub fn listen_address(pad: u8) -> u8 {
    0x20 | (pad & ADDRESS_MASK)
}

/// Talk address for primary address `pad` (our own pad gives MTA).
pub fn talk_address(pad: u8) -> u8 {
    0x40 | (pad & ADDRESS_MASK)
}

// Interrupt endpoint notification bits
pub const AIF_SRQ_BN: u8 = 0;
pub const AIF_WRITE_COMPLETE_BN: u8 = 1;
pub const AIF_READ_COMPLETE_BN: u8 = 2;

// USB IDs. The 82357A and 82357B share the vendor id and register protocol but
// enumerate with different product ids (pre-firmware and post-firmware each).
pub const USB_VID_AGILENT: u16 = 0x0957;
pub const USB_PID_82357B_PREINIT: u16 = 0x0518;
pub const USB_PID_82357B: u16 = 0x0718;
pub const USB_PID_82357A_PREINIT: u16 = 0x0007;
pub const USB_PID_82357A: u16 = 0x0107;

// Endpoint addresses. Bulk-in is shared; the two models differ only in their
// bulk-out and interrupt-in endpoints. USB endpoint addresses encode direction
// in the top bit: 0x80 = IN, 0x00 = OUT. nusb asserts this.
pub const EP_BULK_IN: u8 = 0x82;
pub const EP_82357B_BULK_OUT: u8 = 0x06;
pub const EP_82357B_IRQ_IN: u8 = 0x88;
pub const EP_82357A_BULK_OUT: u8 = 0x04;
pub const EP_82357A_IRQ_IN: u8 = 0x86;

pub const INTERRUPT_BUF_LEN: usize = 8;
pub const STATUS_DATA_LEN: usize = 8;

#[derive(Debug, Clone, Copy, Default)]
pub struct RegisterPairlet {
    pub address: u8,
    pub value: u8,
}

/// Encode a DATA_PIPE_CMD_WR_REGS request.
pub fn encode_wr_regs(regs: &[RegisterPairlet]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + regs.len() * 2);
    out.push(BulkCmd::WrRegs as u8);
    out.push(regs.len() as u8);
    for r in regs {
        out.push(r.address);
        out.push(r.value);
    }
    out
}

/// Validate the bulk-IN response to a WR_REGS command.
pub fn decode_wr_regs_response(resp: &[u8]) -> Result<(), ProtocolError> {
    if resp.len() < 2 {
        return Err(ProtocolError::ShortResponse {
            need: 2,
            got: resp.len(),
        });
    }
    let expected = !(BulkCmd::WrRegs as u8);
    if resp[0] != expected {
        return Err(ProtocolError::BadResponse {
            expected,
            got: resp[0],
        });
    }
    if resp[1] != UGP_SUCCESS {
        return Err(ProtocolError::DeviceError {
            cmd: BulkCmd::WrRegs as u8,
            code: resp[1],
        });
    }
    Ok(())
}

/// Encode a DATA_PIPE_CMD_RD_REGS request.
pub fn encode_rd_regs(addrs: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + addrs.len());
    out.push(BulkCmd::RdRegs as u8);
    out.push(addrs.len() as u8);
    out.extend_from_slice(addrs);
    out
}

/// Parse the bulk-IN response to a RD_REGS command, writing values into `regs`.
pub fn decode_rd_regs_response(
    resp: &[u8],
    regs: &mut [RegisterPairlet],
) -> Result<(), ProtocolError> {
    let need = 2 + regs.len();
    if resp.len() < need {
        return Err(ProtocolError::ShortResponse {
            need,
            got: resp.len(),
        });
    }
    let expected = !(BulkCmd::RdRegs as u8);
    if resp[0] != expected {
        return Err(ProtocolError::BadResponse {
            expected,
            got: resp[0],
        });
    }
    if resp[1] != UGP_SUCCESS {
        return Err(ProtocolError::DeviceError {
            cmd: BulkCmd::RdRegs as u8,
            code: resp[1],
        });
    }
    for (i, r) in regs.iter_mut().enumerate() {
        r.value = resp[2 + i];
    }
    Ok(())
}

/// Encode a DATA_PIPE_CMD_WRITE packet for GPIB data (AWF_NO_ADDRESS always set).
pub fn encode_gpib_write(data: &[u8], send_eoi: bool) -> Vec<u8> {
    let mut flags = WriteFlag::NoAddress as u8 | WriteFlag::NoFastTalkerFirstByte as u8;
    if send_eoi {
        flags |= WriteFlag::SendEoi as u8;
    }
    build_write_packet(flags, data)
}

/// Encode a DATA_PIPE_CMD_WRITE packet for GPIB bus commands (ATN asserted).
pub fn encode_gpib_command(cmd: &[u8]) -> Vec<u8> {
    let flags = WriteFlag::NoAddress as u8 | WriteFlag::Atn as u8 | WriteFlag::NoFastTalker as u8;
    build_write_packet(flags, cmd)
}

fn build_write_packet(flags: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.push(BulkCmd::Write as u8);
    out.push(0);
    out.push(0);
    out.push(flags);
    let len = data.len() as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Encode a DATA_PIPE_CMD_READ request packet.
/// `eos_enabled`: also terminate on `eos_char`.
/// `end_on_eoi` is false for capture. A capture is a stream, not a message:
/// ending the read whenever the talker asserts EOI returns control to the host
/// mid-page and opens a re-arm gap, and a talk-only talker that finds no ready
/// listener in that gap gives up — an HP 53310A reports "printing aborted"
/// partway through. Where a page ends is the client's problem
/// (`docs/CAPTURE.md` §5), so the read should end on length or timeout only.
pub fn encode_gpib_read(
    max_len: u32,
    eos_enabled: bool,
    eos_char: u8,
    end_on_eoi: bool,
) -> Vec<u8> {
    let mut flags = ReadFlag::NoAddress as u8;
    if end_on_eoi {
        flags |= ReadFlag::EndOnEoi as u8;
    }
    if eos_enabled && end_on_eoi {
        flags |= ReadFlag::EndOnEosChar as u8;
    }
    let mut out = Vec::with_capacity(9);
    out.push(BulkCmd::Read as u8);
    out.push(0);
    out.push(0);
    out.push(flags);
    out.extend_from_slice(&max_len.to_le_bytes());
    out.push(eos_char);
    out
}

/// Returns (data_bytes, end_of_message, trailing_flags) from a bulk-IN read
/// response. The last byte of the response is the trailing flags byte; it is
/// stripped from the data and returned so callers can tell *why* the read
/// ended — the non-EOI/EOS reasons (`ATRF_UNADDRESSED` above all) are the
/// diagnostic this backend went a long time without.
pub fn decode_gpib_read_response(raw: &[u8]) -> (Vec<u8>, bool, u8) {
    if raw.is_empty() {
        return (vec![], false, 0);
    }
    let trailing = raw[raw.len() - 1];
    let data = raw[..raw.len() - 1].to_vec();
    let eom = trailing & (ATRF_EOI | ATRF_EOS) != 0;
    (data, eom, trailing)
}

/// Human-readable termination reasons for a trailing flags byte, for logs.
pub fn describe_read_termination(trailing: u8) -> String {
    let names = [
        (ATRF_EOI, "EOI"),
        (ATRF_ATN, "ATN"),
        (ATRF_IFC, "IFC"),
        (ATRF_EOS, "EOS"),
        (ATRF_ABORT, "ABORT"),
        (ATRF_COUNT, "COUNT"),
        (ATRF_DEAD_BUS, "DEAD_BUS"),
        (ATRF_UNADDRESSED, "UNADDRESSED"),
    ];
    let set: Vec<&str> = names
        .iter()
        .filter(|(bit, _)| trailing & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if set.is_empty() {
        format!("{trailing:#04x} (none)")
    } else {
        format!("{trailing:#04x} ({})", set.join("|"))
    }
}
