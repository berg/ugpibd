// SPDX-License-Identifier: GPL-3.0-or-later
use ugpibd::backend::agilent_82357::firmware::parse_hex;
use ugpibd::backend::agilent_82357::{MODEL_82357A, MODEL_82357B};

#[test]
fn model_82357a_has_bundled_firmware() {
    assert!(
        MODEL_82357A.firmware.is_some(),
        "82357A firmware should now be bundled"
    );
}

#[test]
fn per_model_cpucs_addresses() {
    assert_eq!(
        MODEL_82357A.cpucs_addr, 0x7F92,
        "82357A is a first-gen EZ-USB (AN2131)"
    );
    assert_eq!(MODEL_82357B.cpucs_addr, 0xE600, "82357B is FX2");
}

#[test]
fn bundled_82357a_firmware_parses_as_intel_hex() {
    let fw = MODEL_82357A.firmware.expect("bundled A firmware");
    let text = std::str::from_utf8(fw).expect("firmware is UTF-8 hex text");
    let records = parse_hex(text).expect("valid Intel HEX");
    assert!(!records.is_empty(), "expected at least one data record");
}
