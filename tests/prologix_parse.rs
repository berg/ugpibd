// SPDX-License-Identifier: GPL-3.0-or-later
use ugpibd::prologix::{LineResult, PrologixState};

#[test]
fn addr_set_and_query() {
    let mut s = PrologixState::default();
    assert!(matches!(s.handle_line("++addr 15"), LineResult::Ok));
    assert_eq!(s.addr, 15);
    let resp = s.handle_line("++addr");
    assert!(matches!(resp, LineResult::Response(ref r) if r == "15"));
}

#[test]
fn addr_out_of_range() {
    let mut s = PrologixState::default();
    let r = s.handle_line("++addr 31");
    assert!(matches!(r, LineResult::Error(_)));
}

#[test]
fn auto_mode() {
    let mut s = PrologixState::default();
    s.handle_line("++auto 1");
    assert!(s.auto_read);
    s.handle_line("++auto 0");
    assert!(!s.auto_read);
}

#[test]
fn eoi_flag() {
    let mut s = PrologixState::default();
    assert!(s.eoi); // default on
    s.handle_line("++eoi 0");
    assert!(!s.eoi);
}

#[test]
fn eos_values() {
    let mut s = PrologixState::default();
    s.handle_line("++eos 0");
    assert_eq!(s.eos_mode, 0);
    s.handle_line("++eos 2");
    assert_eq!(s.eos_mode, 2);
}

#[test]
fn ver_response_contains_prologix() {
    let mut s = PrologixState::default();
    let r = s.handle_line("++ver");
    match r {
        LineResult::Response(v) => assert!(v.contains("Prologix")),
        _ => panic!("expected Response"),
    }
}

#[test]
fn mode_1_ok() {
    let mut s = PrologixState::default();
    let r = s.handle_line("++mode 1");
    assert!(matches!(r, LineResult::Ok));
}

#[test]
fn mode_0_error() {
    let mut s = PrologixState::default();
    let r = s.handle_line("++mode 0");
    assert!(matches!(r, LineResult::Error(_)));
}

#[test]
fn data_line_returns_forward() {
    let mut s = PrologixState::default();
    s.handle_line("++addr 15");
    let auto = s.auto_read;
    let r = s.handle_line("*IDN?");
    match r {
        LineResult::Forward {
            pad,
            auto_read,
            send_eoi,
            ..
        } => {
            assert_eq!(pad, 15);
            assert_eq!(auto_read, auto);
            assert!(send_eoi);
        }
        _ => panic!("expected Forward"),
    }
}

#[test]
fn data_applies_eos_termination() {
    let mut s = PrologixState {
        eos_mode: 0, // CR+LF
        ..PrologixState::default()
    };
    s.handle_line("++addr 1");
    let r = s.handle_line("MEAS:VOLT?");
    match r {
        LineResult::Forward { data, .. } => {
            assert!(data.ends_with(b"\r\n"), "expected CR+LF, got {data:?}");
        }
        _ => panic!("expected Forward"),
    }
}

#[test]
fn read_command() {
    let mut s = PrologixState::default();
    let r = s.handle_line("++read");
    assert!(matches!(r, LineResult::Read { .. }));
}

#[test]
fn eot_settings() {
    let mut s = PrologixState::default();
    s.handle_line("++eot_enable 1");
    assert!(s.eot_enable);
    s.handle_line("++eot_char 10");
    assert_eq!(s.eot_char, 10);
}

#[test]
fn stub_commands_respond() {
    let mut s = PrologixState::default();
    for cmd in ["++srq", "++spoll", "++llo", "++loc", "++savecfg"] {
        let r = s.handle_line(cmd);
        assert!(
            !matches!(r, LineResult::Forward { .. }),
            "{cmd} should not forward"
        );
    }
}

#[test]
fn clr_command() {
    let mut s = PrologixState::default();
    s.handle_line("++addr 7");
    let r = s.handle_line("++clr");
    assert!(matches!(r, LineResult::DeviceClear { pad: 7 }));
}

#[test]
fn ifc_command() {
    let mut s = PrologixState::default();
    let r = s.handle_line("++ifc");
    assert!(matches!(r, LineResult::Ifc));
}

#[test]
fn rst_command() {
    let mut s = PrologixState::default();
    let r = s.handle_line("++rst");
    assert!(matches!(r, LineResult::Reset));
}

#[test]
fn read_tmo_ms() {
    let mut s = PrologixState::default();
    s.handle_line("++read_tmo_ms 5000");
    assert_eq!(s.read_tmo_ms, 5000);
}

#[test]
fn apply_eot_appends_when_enabled() {
    let s = PrologixState {
        eot_enable: true,
        eot_char: 0x00,
        ..PrologixState::default()
    };
    let out = s.apply_eot(b"hello".to_vec());
    assert_eq!(out, b"hello\0");
}

#[test]
fn apply_eot_noop_when_disabled() {
    let s = PrologixState::default();
    let out = s.apply_eot(b"hello".to_vec());
    assert_eq!(out, b"hello");
}
