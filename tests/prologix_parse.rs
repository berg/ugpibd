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
    for cmd in ["++llo", "++loc", "++savecfg"] {
        let r = s.handle_line(cmd);
        assert!(
            !matches!(r, LineResult::Forward { .. }),
            "{cmd} should not forward"
        );
    }
}

/// `++srq` used to answer a hardcoded "0" and `++spoll`/`++trg` did nothing at
/// all, so a polling script saw a healthy-looking bus that never had news. They
/// must reach the bus now, never synthesise an answer.
#[test]
fn srq_spoll_and_trg_reach_the_bus() {
    let mut s = PrologixState::default();
    assert_eq!(s.handle_line("++srq"), LineResult::Srq);

    s.handle_line("++addr 16");
    // No argument: act on the currently addressed instrument.
    assert_eq!(s.handle_line("++spoll"), LineResult::SerialPoll { pad: 16 });
    assert_eq!(s.handle_line("++trg"), LineResult::Trigger { pad: 16 });

    // Explicit address overrides, without disturbing the addressed instrument.
    assert_eq!(
        s.handle_line("++spoll 9"),
        LineResult::SerialPoll { pad: 9 }
    );
    assert_eq!(s.handle_line("++trg 9"), LineResult::Trigger { pad: 9 });
    assert_eq!(s.addr, 16, "++spoll/++trg must not change the address");
}

#[test]
fn spoll_and_trg_reject_bad_addresses() {
    let mut s = PrologixState::default();
    for cmd in ["++spoll 31", "++spoll x", "++trg 31", "++trg -1"] {
        assert!(
            matches!(s.handle_line(cmd), LineResult::Error(_)),
            "{cmd} should be rejected"
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

#[test]
fn with_addr_sets_initial_address_and_keeps_defaults() {
    let s = PrologixState::with_addr(12);
    assert_eq!(s.addr, 12);
    assert_eq!(s.eos_mode, PrologixState::default().eos_mode);
    assert_eq!(s.eoi, PrologixState::default().eoi);
}

// --- capture-mode commands (ugpibd extensions, not real Prologix) ---

#[test]
fn lines_takes_no_argument() {
    let mut s = PrologixState::default();
    assert!(matches!(s.handle_line("++lines"), LineResult::BusLines));
}

#[test]
fn lon_sets_clears_and_queries() {
    let mut s = PrologixState::default();
    assert!(matches!(
        s.handle_line("++lon 1"),
        LineResult::ListenOnly(Some(true))
    ));
    assert!(matches!(
        s.handle_line("++lon 0"),
        LineResult::ListenOnly(Some(false))
    ));
    // No argument asks rather than assumes: the answer lives in the backend,
    // not in this parser, so it must not be guessed here.
    assert!(matches!(
        s.handle_line("++lon"),
        LineResult::ListenOnly(None)
    ));
}

#[test]
fn lon_rejects_anything_but_0_or_1() {
    let mut s = PrologixState::default();
    for bad in ["++lon 2", "++lon yes", "++lon -1", "++lon 1 1"] {
        assert!(
            matches!(s.handle_line(bad), LineResult::Error(_)),
            "{bad} should have been refused"
        );
    }
}

#[test]
fn dev_takes_an_address_or_off_or_queries() {
    let mut s = PrologixState::default();
    assert!(matches!(
        s.handle_line("++dev 5"),
        LineResult::DeviceMode(Some(Some(5)))
    ));
    assert!(matches!(
        s.handle_line("++dev 0"),
        LineResult::DeviceMode(Some(Some(0)))
    ));
    assert!(matches!(
        s.handle_line("++dev 30"),
        LineResult::DeviceMode(Some(Some(30)))
    ));
    assert!(matches!(
        s.handle_line("++dev off"),
        LineResult::DeviceMode(Some(None))
    ));
    assert!(matches!(
        s.handle_line("++dev"),
        LineResult::DeviceMode(None)
    ));
}

/// 31 is not a primary address — it is the untalk/unlisten code — so it must be
/// refused here for the same reason `++addr 31` is. An instrument in talk-only
/// reports itself as "address 31", and accepting it would invite pointing the
/// daemon at a device that by construction cannot be addressed.
#[test]
fn dev_rejects_31_and_other_non_addresses() {
    let mut s = PrologixState::default();
    for bad in [
        "++dev 31",
        "++dev 255",
        "++dev -1",
        "++dev five",
        "++dev on",
    ] {
        assert!(
            matches!(s.handle_line(bad), LineResult::Error(_)),
            "{bad} should have been refused"
        );
    }
}
