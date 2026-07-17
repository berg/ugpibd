// SPDX-License-Identifier: GPL-3.0-or-later
use ugpibd::firmware::parse_hex;

// Intel HEX checksum = two's complement of sum of all other bytes.
// Record 1 below: byte_count=03, addr=0000, type=00, data=[02,00,0E],
// cksum = 0x100 - (03+02+0E) = 0xED.

#[test]
fn parse_data_record() {
    let records = parse_hex(":0300000002000EED\n:00000001FF\n").unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].address, 0x0000);
    assert_eq!(records[0].data, vec![0x02, 0x00, 0x0e]);
}

#[test]
fn parse_multiple_records() {
    // r1: 02 0000 00 AABB — cksum = 0x100 - (02+AA+BB) mod 256 = 0x99
    // r2: 01 0002 00 CC   — cksum = 0x100 - (01+02+CC)       = 0x31
    let hex = ":02000000AABB99\n:01000200CC31\n:00000001FF\n";
    let records = parse_hex(hex).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].data, vec![0xaa, 0xbb]);
    assert_eq!(records[1].address, 0x0002);
    assert_eq!(records[1].data, vec![0xcc]);
}

#[test]
fn bad_checksum_fails() {
    // Using FF instead of ED
    let result = parse_hex(":0300000002000EFF\n:00000001FF\n");
    assert!(result.is_err(), "expected checksum error");
}

#[test]
fn missing_colon_fails() {
    let result = parse_hex("0300000002000EED\n:00000001FF\n");
    assert!(result.is_err());
}

#[test]
fn empty_input_ok() {
    let records = parse_hex(":00000001FF\n").unwrap();
    assert!(records.is_empty());
}

#[test]
fn ignores_cr_lf() {
    let records = parse_hex(":0300000002000EED\r\n:00000001FF\r\n").unwrap();
    assert_eq!(records.len(), 1);
}

#[test]
fn parses_real_firmware() {
    let hex = include_str!("../firmware/measat_releaseX1.8.hex");
    let records = parse_hex(hex).expect("real firmware must parse");
    assert!(!records.is_empty(), "firmware should produce records");
}
