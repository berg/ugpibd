// SPDX-License-Identifier: GPL-3.0-or-later
//
// Vendored from lxi-rs common/mod.rs (GPL-3.0-or-later).

use bitfield::bitfield;

pub const PROTOCOL_1_0: Protocol = Protocol(0x0100);
pub const PROTOCOL_1_1: Protocol = Protocol(0x0101);
pub const PROTOCOL_2_0: Protocol = Protocol(0x0200);
pub const SUPPORTED_PROTOCOL: Protocol = PROTOCOL_2_0;

bitfield! {
    #[derive(Ord, PartialOrd, Eq, PartialEq, Copy, Clone)]
    pub struct Protocol(u16);
    impl Debug;
    pub u8, major, set_major : 15, 8;
    pub u8, minor, set_minor : 7, 0;
}

impl Protocol {
    pub fn as_parameter(&self, session_id: u16) -> u32 {
        ((self.0 as u32) << 16) | session_id as u32
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major(), self.minor())
    }
}

impl From<u16> for Protocol {
    fn from(x: u16) -> Self {
        Protocol(x)
    }
}

impl From<Protocol> for u16 {
    fn from(p: Protocol) -> Self {
        p.0
    }
}
