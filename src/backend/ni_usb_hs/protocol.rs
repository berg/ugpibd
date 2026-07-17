// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Wire protocol for the NI GPIB-USB-HS, translated from the Linux kernel
// `drivers/gpib/ni_usb/ni_usb_gpib.c` (GPL-2.0-or-later). The adapter is a
// TNT4882 ("TURBO488") operated in NEC7210 one-chip mode; the USB layer wraps
// register and data operations in framed bulk blocks. All framing lives here as
// pure encode/decode so it can be unit-tested without hardware.
//
// EXPERIMENTAL: translated from the C reference and validated only against the
// byte layouts quoted there — not yet exercised on a physical adapter.

use anyhow::{bail, Result};

// ---- Bulk block ids (outgoing opcodes and response markers) ----------------

pub const NIUSB_IBCAC_ID: u8 = 0x01; // take control (assert ATN)
pub const NIUSB_TERM_ID: u8 = 0x04; // bulk termination block
pub const NIUSB_IBGTS_ID: u8 = 0x06; // go to standby (release ATN)
pub const NIUSB_REG_READ_ID: u8 = 0x08;
pub const NIUSB_REG_WRITE_ID: u8 = 0x09;
pub const NIUSB_IBSIC_ID: u8 = 0x0f; // interface clear (IFC pulse)
pub const NIUSB_REGISTER_READ_DATA_START_ID: u8 = 0x34;
pub const NIUSB_REGISTER_READ_DATA_END_ID: u8 = 0x35;
pub const NIUSB_IBRD_DATA_ID: u8 = 0x36; // 15-byte read data block
pub const NIUSB_IBRD_EXTENDED_DATA_ID: u8 = 0x37; // 30-byte read data block
pub const NIUSB_IBRD_STATUS_ID: u8 = 0x38;

// Data-phase opcodes.
pub const NIUSB_DATA_READ_OP: u8 = 0x0a;
pub const NIUSB_COMMAND_OP: u8 = 0x0c;
pub const NIUSB_DATA_WRITE_OP: u8 = 0x0d;

// ---- Logical subdevices addressed by register ops --------------------------

pub const SUBDEV_TNT4882: u8 = 1;
pub const SUBDEV_UNKNOWN2: u8 = 2;
pub const SUBDEV_UNKNOWN3: u8 = 3;

// ---- TNT4882 register offsets (value placed in the packet) -----------------
// NEC7210 logical registers sit at 2x their 7210 number; TNT-native registers
// use their direct offset.

pub const REG_ADMR: u8 = 0x08; // address mode
pub const REG_AUXMR: u8 = 0x0a; // aux mode / aux command
pub const REG_ADR: u8 = 0x0c; // address
pub const REG_SPMR: u8 = 0x06; // serial-poll mode (this board's status byte)
pub const REG_AUXCR: u8 = 0x06; // TNT 9914-mode aux (same offset, init context)
pub const REG_IMR1: u8 = 0x02;
pub const REG_IMR2: u8 = 0x04;
pub const REG_IMR3: u8 = 0x12;
pub const REG_IMR0: u8 = 0x1d;
pub const REG_HSSEL: u8 = 0x0d;
pub const REG_CMDR: u8 = 0x1c; // TNT command register
pub const REG_KEYREG: u8 = 0x17;

// ---- AUXMR command values (written to REG_AUXMR) ---------------------------

pub const AUX_PON: u8 = 0x00; // immediate execute pon
pub const AUX_CPPF: u8 = 0x01; // clear parallel-poll flag
pub const AUX_CR: u8 = 0x02; // chip reset
pub const AUX_DSC: u8 = 0x14;
pub const AUX_CIFC: u8 = 0x16; // clear IFC
pub const AUX_CREN: u8 = 0x17; // clear REN
pub const AUX_SREN: u8 = 0x1f; // set REN
pub const AUX_HLDI: u8 = 0x51; // rfd holdoff immediately
pub const AUX_CLEAR_END: u8 = 0x55;

// AUXMR register-load forms (base | bits).
pub const AUXRA: u8 = 0x80;
pub const HR_HLDA: u8 = 0x01;
pub const HR_BIN: u8 = 0x10;
pub const AUXRB: u8 = 0xa0;
pub const HR_TRI: u8 = 0x04;
pub const AUXRG: u8 = 0x40;
pub const NTNL_BIT: u8 = 0x08;
pub const AUXRI: u8 = 0xe0;
pub const SISB: u8 = 0x01;
pub const USTD: u8 = 0x08;
pub const MSTD: u8 = 0x20; // KEYREG bit

// CMDR values.
pub const CMDR_SOFT_RESET: u8 = 0x22;
pub const CMDR_SETSC: u8 = 0x03; // set system controller
pub const CMDR_CLRSC: u8 = 0x02; // clear system controller

// Misc init constants.
pub const TNT_ONE_CHIP_BIT: u8 = 0x01; // HSSEL
pub const TNT_IMR0_ALWAYS_BITS: u8 = 0x80;

// ADR/ADMR values used when secondary addressing is disabled.
pub const ADR_DISABLE_SAD: u8 = 0xe0; // HR_ARS | HR_DT | HR_DL
pub const ADMR_DISABLE_SAD: u8 = 0x31; // HR_TRM0 | HR_TRM1 | HR_ADM0

// ---- GPIB command bytes (ATN true) -----------------------------------------

pub const GPIB_SDC: u8 = 0x04; // selected device clear
pub const GPIB_GET: u8 = 0x08; // group execute trigger
pub const GPIB_SPE: u8 = 0x18; // serial poll enable
pub const GPIB_SPD: u8 = 0x19; // serial poll disable
pub const GPIB_UNL: u8 = 0x3f; // unlisten
pub const GPIB_UNT: u8 = 0x5f; // untalk

/// Listen-address command byte for primary address `pad`.
pub fn listen_address(pad: u8) -> u8 {
    0x20 | (pad & 0x1f)
}

/// Talk-address command byte for primary address `pad`.
pub fn talk_address(pad: u8) -> u8 {
    0x40 | (pad & 0x1f)
}

/// ibsta END bit (message terminated by EOI or EOS).
pub const IBSTA_END: u16 = 0x2000;

/// One register access: write `value` to (`device`, `address`) or read from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NiRegister {
    pub device: u8,
    pub address: u8,
    pub value: u8,
}

impl NiRegister {
    pub fn new(device: u8, address: u8, value: u8) -> Self {
        Self {
            device,
            address,
            value,
        }
    }
}

/// Decoded 8-byte status block that trails most responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusBlock {
    pub id: u8,
    pub ibsta: u16,
    pub error_code: u8,
    /// Residual (untransferred) byte count: `transferred = requested - count`.
    pub count: u16,
}

/// Push a 4-byte termination block. Returns bytes written.
fn push_termination(buf: &mut Vec<u8>) {
    buf.extend_from_slice(&[NIUSB_TERM_ID, 0x00, 0x00, 0x00]);
}

/// Zero-pad `buf` up to the next 4-byte boundary.
fn pad_to_4(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0x00);
    }
}

/// Parse the 8-byte status block at the front of `buf`. `ibsta` is big-endian;
/// `count` is a little-endian two's-complement (negated) residual count.
pub fn parse_status_block(buf: &[u8]) -> Result<StatusBlock> {
    if buf.len() < 8 {
        bail!("ni status block too short: {} bytes", buf.len());
    }
    let ibsta = (u16::from(buf[1]) << 8) | u16::from(buf[2]);
    let raw_count = u16::from(buf[4]) | (u16::from(buf[5]) << 8);
    let count = (!raw_count).wrapping_add(1); // two's-complement negate
    Ok(StatusBlock {
        id: buf[0],
        ibsta,
        error_code: buf[3],
        count,
    })
}

/// Encode a batch of register writes: `09 N 00`, N `(dev,addr,val)` triples,
/// zero-pad to a 4-byte boundary, then a termination block.
pub fn encode_register_write(writes: &[NiRegister]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(writes.len() * 3 + 0x10);
    buf.push(NIUSB_REG_WRITE_ID);
    buf.push(writes.len() as u8);
    buf.push(0x00);
    for r in writes {
        buf.extend_from_slice(&[r.device, r.address, r.value]);
    }
    pad_to_4(&mut buf);
    push_termination(&mut buf);
    buf
}

/// Parse a register-write response (exactly 16 bytes). Returns the status block
/// and the number of writes the adapter reports completing.
pub fn parse_register_write_response(buf: &[u8]) -> Result<(StatusBlock, u8)> {
    if buf.len() != 16 {
        bail!(
            "ni register-write response must be 16 bytes, got {}",
            buf.len()
        );
    }
    let status = parse_status_block(buf)?;
    let writes_completed = buf[8];
    if status.id != NIUSB_REG_WRITE_ID {
        bail!("ni register-write bad id: {:#04x}", status.id);
    }
    if status.error_code != 0 {
        bail!("ni register-write error code {:#04x}", status.error_code);
    }
    Ok((status, writes_completed))
}

/// Encode a batch of register reads: `08 N`, N `(dev,addr)` pairs, pad, term.
pub fn encode_register_read(reads: &[(u8, u8)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(reads.len() * 2 + 0x10);
    buf.push(NIUSB_REG_READ_ID);
    buf.push(reads.len() as u8);
    for (dev, addr) in reads {
        buf.extend_from_slice(&[*dev, *addr]);
    }
    pad_to_4(&mut buf);
    push_termination(&mut buf);
    buf
}

/// Parse a register-read response into `num` result bytes. Chunks are
/// `[0x34][r0][r1][r2]`, padded to a 4-byte boundary, then `[0x35][count]`.
pub fn parse_register_read_response(buf: &[u8], num: usize) -> Result<Vec<u8>> {
    let mut i = 0usize;
    let mut out = Vec::with_capacity(num);
    while out.len() < num {
        if buf.get(i).copied() != Some(NIUSB_REGISTER_READ_DATA_START_ID) {
            bail!("ni register-read: missing chunk start id at offset {i}");
        }
        i += 1;
        for _ in 0..3 {
            if out.len() == num {
                break;
            }
            let b = *buf
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("ni register-read truncated"))?;
            out.push(b);
            i += 1;
        }
    }
    while i % 4 != 0 {
        i += 1;
    }
    if buf.get(i).copied() != Some(NIUSB_REGISTER_READ_DATA_END_ID) {
        bail!("ni register-read: missing end id at offset {i}");
    }
    Ok(out)
}

/// Encode a GPIB command-byte transfer (`0x0c`, ATN asserted by the adapter).
/// Caller must keep `cmd` to at most 16 bytes per transfer.
pub fn encode_command(cmd: &[u8], timeout_code: u8) -> Vec<u8> {
    let complement = !((cmd.len() as u8).wrapping_sub(1));
    let mut buf = Vec::with_capacity(cmd.len() + 0x10);
    buf.push(NIUSB_COMMAND_OP);
    buf.push(complement);
    buf.push(0x00);
    buf.push(timeout_code);
    buf.extend_from_slice(cmd);
    pad_to_4(&mut buf);
    push_termination(&mut buf);
    buf
}

/// Encode a GPIB data write (`0x0d`), asserting EOI on the last byte if asked.
pub fn encode_data_write(data: &[u8], send_eoi: bool, timeout_code: u8) -> Vec<u8> {
    let complement = !((data.len() as u16).wrapping_sub(1));
    let mut buf = Vec::with_capacity(data.len() + 0x10);
    buf.push(NIUSB_DATA_WRITE_OP);
    buf.push((complement & 0xff) as u8);
    buf.push((complement >> 8) as u8);
    buf.push(timeout_code);
    buf.push(0x00);
    buf.push(0x00);
    buf.push(if send_eoi { 0x08 } else { 0x00 });
    buf.push(0x00);
    buf.extend_from_slice(data);
    pad_to_4(&mut buf);
    push_termination(&mut buf);
    buf
}

/// Parse a data-write / command response (12 bytes) into bytes transferred.
/// `NIUSB_ABORTED_ERROR` (1) is treated as success (interrupted, not fatal).
pub fn parse_write_response(buf: &[u8], requested: usize) -> Result<usize> {
    let status = parse_status_block(buf)?;
    match status.error_code {
        0 | 1 => Ok(requested.saturating_sub(status.count as usize)),
        3 => bail!("ni write: addressing error (no such device)"),
        8 => bail!("ni write: no listener on bus"),
        10 => bail!("ni write: timeout"),
        code => bail!("ni write: error code {code:#04x}"),
    }
}

/// Encode a GPIB data read (`0x0a`). The request embeds a 2-register aux write
/// (holdoff-immediate + clear-end) exactly as the kernel driver does.
pub fn encode_data_read(len: usize, eos_mode: u16, eos_char: u8, timeout_code: u8) -> Vec<u8> {
    let complement = !((len as u16).wrapping_sub(1));
    let mut buf = Vec::with_capacity(0x20);
    buf.push(NIUSB_DATA_READ_OP);
    buf.push((eos_mode >> 8) as u8);
    buf.push(eos_char);
    buf.push(timeout_code);
    buf.push((complement & 0xff) as u8);
    buf.push((complement >> 8) as u8);
    buf.push(0x00);
    buf.push(0x00);
    // Embedded register-write block: 2 aux commands to the TNT4882.
    buf.push(NIUSB_REG_WRITE_ID);
    buf.push(0x02);
    buf.push(0x00);
    buf.extend_from_slice(&[SUBDEV_TNT4882, REG_AUXMR, AUX_HLDI]);
    buf.extend_from_slice(&[SUBDEV_TNT4882, REG_AUXMR, AUX_CLEAR_END]);
    pad_to_4(&mut buf);
    push_termination(&mut buf);
    buf
}

/// Parse a data-read response: one or more `0x36`/`0x37` data blocks, an `0x38`
/// status block, a trailing count, then an embedded register-write status.
/// Returns the received bytes (truncated to the real length) and `end` (EOI/EOS
/// seen), derived from the status block's END bit.
pub fn parse_data_read_response(buf: &[u8], max_len: usize) -> Result<(Vec<u8>, bool)> {
    let mut i = 0usize;
    let mut data = Vec::with_capacity(max_len.min(buf.len()));
    let mut num_blocks = 0usize;
    let mut block_len = 0usize;

    loop {
        match buf.get(i).copied() {
            Some(NIUSB_IBRD_DATA_ID) => {
                block_len = 15;
                i += 1;
            }
            Some(NIUSB_IBRD_EXTENDED_DATA_ID) => {
                block_len = 30;
                i += 1;
                // Extended blocks carry one extra zero byte before the payload.
                if buf.get(i).copied() != Some(0) {
                    bail!("ni read: extended block missing zero byte at {i}");
                }
                i += 1;
            }
            _ => break,
        }
        for _ in 0..block_len {
            let b = *buf
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("ni read: data block truncated"))?;
            data.push(b);
            i += 1;
        }
        num_blocks += 1;
    }

    let status = parse_status_block(buf.get(i..).unwrap_or(&[]))?;
    if status.id != NIUSB_IBRD_STATUS_ID {
        bail!("ni read: bad status id {:#04x}", status.id);
    }
    i += 8;
    // One reserved byte, then the trailing real-length count for the last block.
    i += 1;
    let actual = if num_blocks > 0 {
        let tail = *buf
            .get(i)
            .ok_or_else(|| anyhow::anyhow!("ni read: missing length byte"))?;
        (num_blocks - 1) * block_len + tail as usize
    } else {
        0
    };

    data.truncate(actual.min(max_len));
    let end = status.ibsta & IBSTA_END != 0;
    Ok((data, end))
}

/// A simple single-block op packet: `id 00 00 00` + termination. Covers
/// take-control (with a sync flag), go-to-standby, and interface-clear.
fn encode_simple_op(id: u8, arg: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&[id, arg, 0x00, 0x00]);
    push_termination(&mut buf);
    buf
}

/// Take control of the bus (assert ATN). `synchronous` selects a synchronous
/// take-control when true.
pub fn encode_take_control(synchronous: bool) -> Vec<u8> {
    encode_simple_op(NIUSB_IBCAC_ID, if synchronous { 0x01 } else { 0x00 })
}

/// Release ATN and go to standby.
pub fn encode_go_to_standby() -> Vec<u8> {
    encode_simple_op(NIUSB_IBGTS_ID, 0x00)
}

/// Pulse Interface Clear.
pub fn encode_interface_clear() -> Vec<u8> {
    encode_simple_op(NIUSB_IBSIC_ID, 0x00)
}

/// Map a millisecond GPIB timeout to the adapter's timeout code byte.
///
/// TRANSLATED, UNVERIFIED: the kernel `ni_usb_timeout_code` table was not
/// captured during porting, so this uses a conservative fixed code that should
/// give a multi-second bus timeout. Revisit against hardware.
pub fn timeout_code(_timeout_ms: u32) -> u8 {
    0x00
}

/// The 26-register init sequence bringing the TNT4882 up as system controller
/// at primary address `pad`, with secondary addressing disabled, T1 = 500 ns,
/// and EOS mode `eos_mode`. Transcribed from `ni_usb_setup_init`.
///
/// TRANSLATED, UNVERIFIED on hardware — the ordering and values mirror the C
/// reference but have not been exercised on a physical adapter.
pub fn setup_init(pad: u8, eos_mode: u16) -> Vec<NiRegister> {
    let tnt = SUBDEV_TNT4882;
    let auxra = AUXRA | HR_HLDA | if eos_mode & 0x0400 != 0 { HR_BIN } else { 0 };

    let mut regs = vec![
        NiRegister::new(SUBDEV_UNKNOWN3, 0x10, 0x00),
        NiRegister::new(tnt, REG_CMDR, CMDR_SOFT_RESET),
        NiRegister::new(tnt, REG_AUXMR, auxra),
        NiRegister::new(tnt, REG_AUXCR, auxra),
        NiRegister::new(tnt, REG_HSSEL, TNT_ONE_CHIP_BIT),
        NiRegister::new(tnt, REG_AUXMR, AUX_CR),
        NiRegister::new(tnt, REG_IMR0, TNT_IMR0_ALWAYS_BITS),
        NiRegister::new(tnt, REG_IMR1, 0x00),
        NiRegister::new(tnt, REG_IMR2, 0x00),
        NiRegister::new(tnt, REG_IMR3, 0x00),
        NiRegister::new(tnt, REG_AUXMR, AUX_HLDI),
    ];
    regs.extend(setup_t1_delay(500));
    regs.push(NiRegister::new(tnt, REG_AUXMR, AUXRG | NTNL_BIT));
    regs.push(NiRegister::new(tnt, REG_CMDR, CMDR_SETSC)); // master / system controller
    regs.push(NiRegister::new(tnt, REG_AUXMR, AUX_CIFC));
    regs.push(NiRegister::new(tnt, REG_ADR, pad & 0x1f));
    regs.push(NiRegister::new(SUBDEV_UNKNOWN2, 0x00, pad & 0x1f));
    regs.extend(setup_disable_sad());
    regs.push(NiRegister::new(SUBDEV_UNKNOWN2, 0x02, 0xfd));
    regs.push(NiRegister::new(tnt, 0x0f, 0x11));
    regs.push(NiRegister::new(tnt, REG_AUXMR, AUX_PON));
    regs.push(NiRegister::new(tnt, REG_AUXMR, AUX_CPPF));
    regs
}

/// T1 source/settle-time register writes. See caveat on [`setup_init`].
fn setup_t1_delay(ns: u32) -> Vec<NiRegister> {
    let tnt = SUBDEV_TNT4882;
    let auxri = AUXRI | SISB | if ns <= 1100 { USTD } else { 0 };
    let auxrb = AUXRB | if ns <= 500 { HR_TRI } else { 0 };
    let keyreg = if ns <= 350 { MSTD } else { 0x00 };
    vec![
        NiRegister::new(tnt, REG_AUXMR, auxri),
        NiRegister::new(tnt, REG_AUXMR, auxrb),
        NiRegister::new(tnt, REG_KEYREG, keyreg),
    ]
}

/// Register writes that disable secondary addressing (the common case).
fn setup_disable_sad() -> Vec<NiRegister> {
    vec![
        NiRegister::new(SUBDEV_TNT4882, REG_ADR, ADR_DISABLE_SAD),
        NiRegister::new(SUBDEV_TNT4882, REG_ADMR, ADMR_DISABLE_SAD),
        NiRegister::new(SUBDEV_UNKNOWN2, 0x01, 0x00),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_4_aligned_terminated(buf: &[u8]) {
        assert_eq!(buf.len() % 4, 0, "packet not 4-byte aligned: {buf:02x?}");
        let tail = &buf[buf.len() - 4..];
        assert_eq!(tail, [NIUSB_TERM_ID, 0, 0, 0], "missing termination");
    }

    #[test]
    fn register_write_framing() {
        // Two writes -> 09 02 00, two triples, pad to mod-4, termination.
        let regs = [
            NiRegister::new(SUBDEV_TNT4882, REG_AUXMR, AUX_HLDI),
            NiRegister::new(SUBDEV_TNT4882, REG_AUXMR, AUX_CLEAR_END),
        ];
        let buf = encode_register_write(&regs);
        assert_eq!(&buf[..3], &[NIUSB_REG_WRITE_ID, 0x02, 0x00]);
        assert_eq!(&buf[3..6], &[SUBDEV_TNT4882, REG_AUXMR, AUX_HLDI]);
        assert_eq!(&buf[6..9], &[SUBDEV_TNT4882, REG_AUXMR, AUX_CLEAR_END]);
        is_4_aligned_terminated(&buf);
    }

    #[test]
    fn status_block_endianness_and_count() {
        // ibsta big-endian (0x2000 END), error 0, residual count 3 -> raw = -3.
        let raw_count = (-3i16) as u16;
        let block = [
            NIUSB_REG_WRITE_ID,
            0x20,
            0x00, // ibsta = 0x2000
            0x00, // error
            (raw_count & 0xff) as u8,
            (raw_count >> 8) as u8,
            0x00,
            0x00,
        ];
        let s = parse_status_block(&block).unwrap();
        assert_eq!(s.id, NIUSB_REG_WRITE_ID);
        assert_eq!(s.ibsta, 0x2000);
        assert_eq!(s.error_code, 0);
        assert_eq!(s.count, 3);
    }

    #[test]
    fn register_write_response_ok() {
        let mut block = vec![NIUSB_REG_WRITE_ID, 0, 0, 0, 0, 0, 0, 0];
        block.push(2); // writes completed
        block.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]); // pad to 16
        let (status, completed) = parse_register_write_response(&block).unwrap();
        assert_eq!(status.id, NIUSB_REG_WRITE_ID);
        assert_eq!(completed, 2);
    }

    #[test]
    fn register_write_response_rejects_error_code() {
        let mut block = vec![NIUSB_REG_WRITE_ID, 0, 0, 0x05, 0, 0, 0, 0];
        block.extend_from_slice(&[0; 8]);
        assert!(parse_register_write_response(&block).is_err());
    }

    #[test]
    fn data_write_header_and_eoi() {
        let buf = encode_data_write(b"*IDN?", true, 0);
        assert_eq!(buf[0], NIUSB_DATA_WRITE_OP);
        // ~(5-1) = ~4 = 0xfffb little-endian.
        let complement = !(5u16 - 1);
        assert_eq!(buf[1], (complement & 0xff) as u8);
        assert_eq!(buf[2], (complement >> 8) as u8);
        assert_eq!(buf[6], 0x08, "EOI flag");
        assert_eq!(&buf[8..13], b"*IDN?");
        is_4_aligned_terminated(&buf);

        let no_eoi = encode_data_write(b"X", false, 0);
        assert_eq!(no_eoi[6], 0x00, "no EOI flag");
    }

    #[test]
    fn data_read_embeds_aux_writes() {
        let buf = encode_data_read(64, 0, b'\n', 0);
        assert_eq!(buf[0], NIUSB_DATA_READ_OP);
        assert_eq!(buf[2], b'\n', "eos char");
        // Embedded 2-register write block at offset 8.
        assert_eq!(&buf[8..11], &[NIUSB_REG_WRITE_ID, 0x02, 0x00]);
        assert_eq!(&buf[11..14], &[SUBDEV_TNT4882, REG_AUXMR, AUX_HLDI]);
        assert_eq!(&buf[14..17], &[SUBDEV_TNT4882, REG_AUXMR, AUX_CLEAR_END]);
        is_4_aligned_terminated(&buf);
    }

    #[test]
    fn command_framing_complement_count() {
        let buf = encode_command(&[GPIB_UNL, listen_address(23), talk_address(0)], 0);
        assert_eq!(buf[0], NIUSB_COMMAND_OP);
        assert_eq!(buf[1], !(3u8 - 1)); // ~2
        assert_eq!(&buf[4..7], &[GPIB_UNL, 0x20 | 23, 0x40]);
        is_4_aligned_terminated(&buf);
    }

    #[test]
    fn simple_ops_layout() {
        assert_eq!(
            encode_take_control(true),
            [NIUSB_IBCAC_ID, 1, 0, 0, 4, 0, 0, 0]
        );
        assert_eq!(encode_take_control(false)[1], 0);
        assert_eq!(
            encode_go_to_standby(),
            [NIUSB_IBGTS_ID, 0, 0, 0, 4, 0, 0, 0]
        );
        assert_eq!(
            encode_interface_clear(),
            [NIUSB_IBSIC_ID, 0, 0, 0, 4, 0, 0, 0]
        );
    }

    #[test]
    fn parse_single_read_block_with_end() {
        // One 0x36 block of 15 bytes, real length 5 ("HELLO"), END set.
        let mut resp = vec![NIUSB_IBRD_DATA_ID];
        let mut payload = b"HELLO".to_vec();
        payload.resize(15, 0);
        resp.extend_from_slice(&payload);
        // status block id 0x38, ibsta END = 0x2000.
        resp.extend_from_slice(&[NIUSB_IBRD_STATUS_ID, 0x20, 0x00, 0, 0, 0, 0, 0]);
        resp.push(0x00); // reserved
        resp.push(0x05); // trailing real length of last block
        let (data, end) = parse_data_read_response(&resp, 64).unwrap();
        assert_eq!(data, b"HELLO");
        assert!(end);
    }

    #[test]
    fn parse_register_read_chunks() {
        // Two results across one chunk: 0x34 r0 r1 r2 (only first two wanted),
        // pad to mod-4, then 0x35 count.
        let resp = [
            NIUSB_REGISTER_READ_DATA_START_ID,
            0xaa,
            0xbb,
            0xcc,
            NIUSB_REGISTER_READ_DATA_END_ID,
            0x02,
            0x00,
            0x00,
        ];
        let vals = parse_register_read_response(&resp, 2).unwrap();
        assert_eq!(vals, vec![0xaa, 0xbb]);
    }

    #[test]
    fn init_sequence_shape() {
        let regs = setup_init(0, 0);
        assert_eq!(regs.len(), 26, "expected 26 init writes");
        // First is the UNKNOWN3 power register, second a TNT soft reset.
        assert_eq!(regs[0], NiRegister::new(SUBDEV_UNKNOWN3, 0x10, 0x00));
        assert_eq!(
            regs[1],
            NiRegister::new(SUBDEV_TNT4882, REG_CMDR, CMDR_SOFT_RESET)
        );
        // System-controller write present.
        assert!(regs
            .iter()
            .any(|r| r.address == REG_CMDR && r.value == CMDR_SETSC));
        // Primary address programmed.
        assert!(regs
            .iter()
            .any(|r| r.device == SUBDEV_TNT4882 && r.address == REG_ADR && r.value == 0));
    }
}
