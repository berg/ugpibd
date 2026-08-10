// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
pub mod backend;
pub mod capture;
pub mod frontend;
pub mod hislip;
pub mod prologix;
pub mod server;

/// Version of this build, from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Help template for both binaries. clap 4 drops the `name version` header that
/// clap 3 printed, and that line is the first thing wanted in a bug report --
/// this restores it while leaving the rest of the default layout alone.
///
/// Shared so `ugpibd --help` and `ugpibd-scpi --help` cannot drift apart.
pub const HELP_TEMPLATE: &str = "\
{name} {version}
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}";
