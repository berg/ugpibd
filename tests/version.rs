// SPDX-License-Identifier: GPL-3.0-or-later
//
// Both binaries must report the build version, via --version and in --help.
//
// The --help case is the fragile one: clap 4 dropped the `name version` header
// that clap 3 printed, so it is only there because of the shared HELP_TEMPLATE.
// Removing or overriding that template would silently lose it again.

use std::process::Command;

fn run(bin: &str, arg: &str) -> String {
    let out = Command::new(bin)
        .arg(arg)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {bin} {arg}: {e}"));
    assert!(
        out.status.success(),
        "{bin} {arg} exited with {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn version_flag_reports_crate_version() {
    for (bin, name) in [
        (env!("CARGO_BIN_EXE_ugpibd"), "ugpibd"),
        (env!("CARGO_BIN_EXE_ugpibd-scpi"), "ugpibd-scpi"),
    ] {
        let stdout = run(bin, "--version");
        let expected = format!("{name} {}\n", ugpibd::VERSION);
        assert_eq!(
            stdout, expected,
            "{name} --version should print exactly {expected:?}"
        );
    }
}

#[test]
fn help_includes_version() {
    for (bin, name) in [
        (env!("CARGO_BIN_EXE_ugpibd"), "ugpibd"),
        (env!("CARGO_BIN_EXE_ugpibd-scpi"), "ugpibd-scpi"),
    ] {
        let stdout = run(bin, "--help");
        let header = format!("{name} {}", ugpibd::VERSION);
        assert!(
            stdout.starts_with(&header),
            "{name} --help should start with {header:?}, got: {:?}",
            stdout.lines().next()
        );
        // The rest of the default layout must survive the custom template.
        assert!(
            stdout.contains(&format!("Usage: {name}")),
            "{name} --help lost its usage line"
        );
        assert!(
            stdout.contains("--help"),
            "{name} --help lost its options list"
        );
    }
}

#[test]
fn version_is_not_a_placeholder() {
    // Guards against CARGO_PKG_VERSION resolving to something useless, which
    // would make every report ambiguous.
    assert!(!ugpibd::VERSION.is_empty());
    assert_ne!(ugpibd::VERSION, "0.0.0");
    assert!(
        ugpibd::VERSION.split('.').count() >= 3,
        "expected an X.Y.Z version, got {:?}",
        ugpibd::VERSION
    );
}
