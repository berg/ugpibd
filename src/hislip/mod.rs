// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors
//
// HiSLIP (IVI-6.1, High-Speed LAN Instrument Protocol) server.
//
// The message codec (messages.rs, errors.rs, protocol.rs) is vendored and
// adapted from lxi-rs (https://github.com/Atmelfan/lxi-rs), which is
// GPL-3.0-or-later. Original author: Gustav Palmqvist.

pub mod errors;
pub mod instrument;
pub mod messages;
pub mod protocol;
pub mod server;

/// IANA-assigned HiSLIP port.
pub const STANDARD_PORT: u16 = 4880;
/// Sub-address used if the client does not specify one.
pub const DEFAULT_SUBADDRESS: &str = "hislip0";
