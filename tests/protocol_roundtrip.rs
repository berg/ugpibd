// SPDX-License-Identifier: GPL-3.0-or-later
use ugpibd::protocol::*;

#[test]
fn wr_regs_roundtrip() {
    let regs = vec![
        RegisterPairlet {
            address: 0x0a,
            value: 0x01,
        },
        RegisterPairlet {
            address: 0x0b,
            value: 0x20,
        },
    ];
    let encoded = encode_wr_regs(&regs);
    assert_eq!(encoded[0], BulkCmd::WrRegs as u8);
    assert_eq!(encoded[1], 2);
    assert_eq!(encoded[2], 0x0a);
    assert_eq!(encoded[3], 0x01);
    assert_eq!(encoded[4], 0x0b);
    assert_eq!(encoded[5], 0x20);
}

#[test]
fn wr_regs_response_ok() {
    let resp = [!(BulkCmd::WrRegs as u8), 0x00, 0, 0, 0, 0, 0, 0];
    assert!(decode_wr_regs_response(&resp).is_ok());
}

#[test]
fn wr_regs_response_error() {
    let resp = [!(BulkCmd::WrRegs as u8), 0x01, 0, 0, 0, 0, 0, 0];
    assert!(decode_wr_regs_response(&resp).is_err());
}

#[test]
fn rd_regs_request() {
    let addrs = [0x0a_u8, 0x0b];
    let encoded = encode_rd_regs(&addrs);
    assert_eq!(encoded[0], BulkCmd::RdRegs as u8);
    assert_eq!(encoded[1], 2);
    assert_eq!(encoded[2], 0x0a);
    assert_eq!(encoded[3], 0x0b);
}

#[test]
fn rd_regs_response_ok() {
    let resp = [!(BulkCmd::RdRegs as u8), 0x00, 0x42, 0x13];
    let mut regs = vec![
        RegisterPairlet {
            address: 0x0a,
            value: 0,
        },
        RegisterPairlet {
            address: 0x0b,
            value: 0,
        },
    ];
    decode_rd_regs_response(&resp, &mut regs).unwrap();
    assert_eq!(regs[0].value, 0x42);
    assert_eq!(regs[1].value, 0x13);
}

#[test]
fn gpib_write_packet() {
    let data = b"*IDN?\n";
    let pkt = encode_gpib_write(data, true);
    assert_eq!(pkt[0], BulkCmd::Write as u8);
    assert_eq!(pkt[1], 0);
    assert_eq!(pkt[2], 0);
    let flags = pkt[3];
    assert!(flags & WriteFlag::NoAddress as u8 != 0);
    assert!(flags & WriteFlag::SendEoi as u8 != 0);
    assert!(flags & WriteFlag::NoFastTalkerFirstByte as u8 != 0);
    assert_eq!(
        u32::from_le_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]),
        data.len() as u32
    );
    assert_eq!(&pkt[8..], data);
}

#[test]
fn gpib_command_packet() {
    let cmd = [0x3f_u8]; // UNL
    let pkt = encode_gpib_command(&cmd);
    assert_eq!(pkt[0], BulkCmd::Write as u8);
    let flags = pkt[3];
    assert!(flags & WriteFlag::NoAddress as u8 != 0);
    assert!(flags & WriteFlag::Atn as u8 != 0);
    assert!(flags & WriteFlag::NoFastTalker as u8 != 0);
}

#[test]
fn gpib_read_packet_eoi_only() {
    let pkt = encode_gpib_read(512, false, 0x0a);
    assert_eq!(pkt[0], BulkCmd::Read as u8);
    let flags = pkt[3];
    assert!(flags & ReadFlag::EndOnEoi as u8 != 0);
    assert!(flags & ReadFlag::NoAddress as u8 != 0);
    assert!(flags & ReadFlag::EndOnEosChar as u8 == 0);
    assert_eq!(u32::from_le_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]), 512);
    assert_eq!(pkt[8], 0x0a);
}

#[test]
fn gpib_read_packet_with_eos() {
    let pkt = encode_gpib_read(512, true, b'\n');
    let flags = pkt[3];
    assert!(flags & ReadFlag::EndOnEosChar as u8 != 0);
    assert_eq!(pkt[8], b'\n');
}
