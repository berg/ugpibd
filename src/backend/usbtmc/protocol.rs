// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// USBTMC 1.0 and USB488 wire format: bulk message headers, class control
// requests, status codes, and the capabilities block. Pure encode/decode with
// no I/O, so all of it is unit-testable without a device.
//
// References: USBTMC Revision 1.0 (USB-IF, 2003) §3 for the bulk framing and
// §4.2 for the control requests; USBTMC-USB488 Subclass Specification 1.0 for
// the TRIGGER message, the READ_STATUS_BYTE / REN_CONTROL / GO_TO_LOCAL /
// LOCAL_LOCKOUT requests and the interrupt-IN notification format.

use anyhow::{bail, Result};

/// Every bulk message begins with a 12-byte header.
pub const HEADER_LEN: usize = 12;

// Bulk-OUT MsgIDs (USBTMC table 2).
pub const MSGID_DEV_DEP_MSG_OUT: u8 = 1;
pub const MSGID_REQUEST_DEV_DEP_MSG_IN: u8 = 2;
/// USB488: Group Execute Trigger as a bulk-OUT message with no payload.
pub const MSGID_TRIGGER: u8 = 128;

// Bulk-IN MsgIDs (USBTMC table 3). The response to a REQUEST_DEV_DEP_MSG_IN
// carries the same id as the request.
pub const MSGID_DEV_DEP_MSG_IN: u8 = 2;

// Class-specific control requests (USBTMC table 15, USB488 table 9).
pub const REQ_INITIATE_ABORT_BULK_OUT: u8 = 1;
pub const REQ_CHECK_ABORT_BULK_OUT_STATUS: u8 = 2;
pub const REQ_INITIATE_ABORT_BULK_IN: u8 = 3;
pub const REQ_CHECK_ABORT_BULK_IN_STATUS: u8 = 4;
pub const REQ_INITIATE_CLEAR: u8 = 5;
pub const REQ_CHECK_CLEAR_STATUS: u8 = 6;
pub const REQ_GET_CAPABILITIES: u8 = 7;
pub const REQ_READ_STATUS_BYTE: u8 = 128;
pub const REQ_REN_CONTROL: u8 = 160;
pub const REQ_GO_TO_LOCAL: u8 = 161;
pub const REQ_LOCAL_LOCKOUT: u8 = 162;

// USBTMC_status values (USBTMC table 16).
pub const STATUS_SUCCESS: u8 = 0x01;
pub const STATUS_PENDING: u8 = 0x02;
pub const STATUS_FAILED: u8 = 0x80;
pub const STATUS_TRANSFER_NOT_IN_PROGRESS: u8 = 0x81;
pub const STATUS_SPLIT_NOT_IN_PROGRESS: u8 = 0x82;
pub const STATUS_SPLIT_IN_PROGRESS: u8 = 0x83;

/// Interrupt-IN `bNotify1` for a service request (USB488 §3.4.1): the second
/// byte is the status byte the device would return to a serial poll.
pub const NOTIFY_SRQ: u8 = 0x81;

/// USB interface class/subclass identifying a USBTMC interface, and the
/// protocol code for the USB488 subclass on top of it.
pub const USB_CLASS_APPLICATION_SPECIFIC: u8 = 0xfe;
pub const USB_SUBCLASS_USBTMC: u8 = 0x03;
pub const USB_PROTOCOL_USB488: u8 = 0x01;

/// Who a class control request is addressed to. Mirrors the two recipients the
/// USBTMC requests use, without dragging nusb's types into the transport trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipient {
    Interface,
    /// The abort requests go to the bulk endpoint they abort.
    Endpoint,
}

/// The next bulk-message bTag. Tags run 1..=255; 0 is reserved.
pub fn next_tag(tag: u8) -> u8 {
    if tag == u8::MAX {
        1
    } else {
        tag + 1
    }
}

/// The next READ_STATUS_BYTE bTag. USB488 §4.3.1.2 confines these to
/// 2..=127 so that `0x80 | bTag` on the interrupt endpoint can never collide
/// with the service-request notification, `0x81`.
pub fn next_status_tag(tag: u8) -> u8 {
    if tag >= 127 {
        2
    } else {
        tag.max(1) + 1
    }
}

/// Bytes of zero padding that bring `len` to a multiple of four. Every bulk-OUT
/// transfer must be padded this way (USBTMC §3.2.1.1), and bulk-IN transfers
/// arrive padded the same way.
pub fn pad_len(len: usize) -> usize {
    (4 - len % 4) % 4
}

fn header(msg_id: u8, tag: u8, transfer_size: u32, attrs: u8, b9: u8) -> [u8; HEADER_LEN] {
    let size = transfer_size.to_le_bytes();
    [
        msg_id, tag, !tag, 0, size[0], size[1], size[2], size[3], attrs, b9, 0, 0,
    ]
}

/// A DEV_DEP_MSG_OUT carrying `data`, with EOM set when `eom`. Returns the
/// full bulk-OUT transfer, padding included.
pub fn encode_dev_dep_msg_out(tag: u8, data: &[u8], eom: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len() + 3);
    out.extend_from_slice(&header(
        MSGID_DEV_DEP_MSG_OUT,
        tag,
        data.len() as u32,
        u8::from(eom),
        0,
    ));
    out.extend_from_slice(data);
    out.extend(std::iter::repeat_n(0u8, pad_len(data.len())));
    out
}

/// A REQUEST_DEV_DEP_MSG_IN asking for up to `max_len` bytes, terminating
/// early on `term_char` when one is given (only valid on devices whose
/// capabilities advertise TermChar support).
pub fn encode_request_dev_dep_msg_in(tag: u8, max_len: u32, term_char: Option<u8>) -> Vec<u8> {
    let (attrs, tc) = match term_char {
        Some(c) => (0x02, c),
        None => (0, 0),
    };
    header(MSGID_REQUEST_DEV_DEP_MSG_IN, tag, max_len, attrs, tc).to_vec()
}

/// The USB488 TRIGGER message: a bare header, no payload.
pub fn encode_trigger(tag: u8) -> Vec<u8> {
    header(MSGID_TRIGGER, tag, 0, 0, 0).to_vec()
}

/// A decoded DEV_DEP_MSG_IN transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InMessage {
    pub data: Vec<u8>,
    /// EOM: the device has finished the message. Without it, another
    /// REQUEST_DEV_DEP_MSG_IN continues the same message.
    pub eom: bool,
    /// The transfer ended because the requested TermChar was seen.
    pub term_char_seen: bool,
    /// `TransferSize` from the header, so a caller can tell a complete transfer
    /// from one the device cut short.
    pub transfer_size: usize,
}

/// Decode a bulk-IN transfer that should answer the request tagged `tag`.
///
/// A header with the wrong MsgID or tag answers some *other* request — the
/// desync case — and is refused rather than handed up as data. A transfer
/// shorter than its own header claims is accepted and reported through
/// `transfer_size`, which is how some firmware ends a message.
pub fn parse_dev_dep_msg_in(buf: &[u8], tag: u8) -> Result<InMessage> {
    if buf.len() < HEADER_LEN {
        bail!(
            "usbtmc bulk-in transfer of {} bytes is shorter than the 12-byte header",
            buf.len()
        );
    }
    if buf[0] != MSGID_DEV_DEP_MSG_IN {
        bail!(
            "usbtmc bulk-in MsgID {:#04x}, expected DEV_DEP_MSG_IN ({:#04x})",
            buf[0],
            MSGID_DEV_DEP_MSG_IN
        );
    }
    if buf[1] != tag {
        bail!(
            "usbtmc bulk-in answers bTag {} but request was {tag}: the pipe is out of step",
            buf[1]
        );
    }
    if buf[2] != !tag {
        bail!(
            "usbtmc bulk-in bTagInverse {:#04x} does not match bTag {tag}",
            buf[2]
        );
    }
    let transfer_size = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let attrs = buf[8];
    let available = buf.len() - HEADER_LEN;
    let take = transfer_size.min(available);
    Ok(InMessage {
        data: buf[HEADER_LEN..HEADER_LEN + take].to_vec(),
        eom: attrs & 0x01 != 0,
        term_char_seen: attrs & 0x02 != 0,
        transfer_size,
    })
}

/// What the interface said it can do, from GET_CAPABILITIES.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub bcd_usbtmc: u16,
    /// The device honors TermChar in REQUEST_DEV_DEP_MSG_IN.
    pub term_char: bool,
    /// `0` when the interface does not implement the USB488 subclass at all.
    pub bcd_usb488: u16,
    /// The interface is a 488.2 device (USB488 table 8, bit 2).
    pub ieee_488_2: bool,
    /// REN_CONTROL, GO_TO_LOCAL and LOCAL_LOCKOUT are accepted (bit 1).
    pub remote_local: bool,
    /// The TRIGGER bulk message is accepted (bit 0).
    pub trigger: bool,
    /// Device capabilities (USB488 table 8, second byte): SCPI, SR1, RL1, DT1.
    pub scpi: bool,
    pub sr1: bool,
    pub rl1: bool,
    pub dt1: bool,
}

/// Decode a GET_CAPABILITIES response. USBTMC defines 12 bytes; USB488 extends
/// it to 24 with its own block at offset 12. A response that stops at the
/// USBTMC part is taken as "no USB488 capabilities", not as an error, since
/// that is what a plain USBTMC device says.
pub fn parse_capabilities(buf: &[u8]) -> Result<Capabilities> {
    if buf.len() < 6 {
        bail!(
            "usbtmc GET_CAPABILITIES reply of {} bytes is too short",
            buf.len()
        );
    }
    if buf[0] != STATUS_SUCCESS {
        bail!("usbtmc GET_CAPABILITIES failed with status {:#04x}", buf[0]);
    }
    let mut caps = Capabilities {
        bcd_usbtmc: u16::from_le_bytes([buf[2], buf[3]]),
        term_char: buf[5] & 0x01 != 0,
        ..Default::default()
    };
    if buf.len() >= 16 {
        caps.bcd_usb488 = u16::from_le_bytes([buf[12], buf[13]]);
        let iface = buf[14];
        let dev = buf[15];
        caps.ieee_488_2 = iface & 0x04 != 0;
        caps.remote_local = iface & 0x02 != 0;
        caps.trigger = iface & 0x01 != 0;
        caps.scpi = dev & 0x08 != 0;
        caps.sr1 = dev & 0x04 != 0;
        caps.rl1 = dev & 0x02 != 0;
        caps.dt1 = dev & 0x01 != 0;
    }
    Ok(caps)
}

/// Check the leading USBTMC_status byte of a control-request reply.
pub fn check_status(what: &str, reply: &[u8]) -> Result<()> {
    match reply.first() {
        Some(&STATUS_SUCCESS) => Ok(()),
        Some(&s) => bail!("usbtmc {what} failed with status {}", status_name(s)),
        None => bail!("usbtmc {what}: empty reply"),
    }
}

/// Human-readable status code, with the raw value kept in view.
pub fn status_name(status: u8) -> String {
    let name = match status {
        STATUS_SUCCESS => "SUCCESS",
        STATUS_PENDING => "PENDING",
        STATUS_FAILED => "FAILED",
        STATUS_TRANSFER_NOT_IN_PROGRESS => "TRANSFER_NOT_IN_PROGRESS",
        STATUS_SPLIT_NOT_IN_PROGRESS => "SPLIT_NOT_IN_PROGRESS",
        STATUS_SPLIT_IN_PROGRESS => "SPLIT_IN_PROGRESS",
        _ => "unknown",
    };
    format!("{status:#04x} ({name})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_skip_zero() {
        assert_eq!(next_tag(1), 2);
        assert_eq!(next_tag(255), 1);
        assert_eq!(next_status_tag(2), 3);
        assert_eq!(next_status_tag(127), 2);
        assert_eq!(next_status_tag(0), 2);
        // Never 0x01: 0x80 | 1 would read as the SRQ notification.
        for t in 0..=200u8 {
            let n = next_status_tag(t);
            assert!((2..=127).contains(&n), "status tag {n} out of range");
        }
    }

    #[test]
    fn dev_dep_msg_out_is_padded_to_four_and_flags_eom() {
        let m = encode_dev_dep_msg_out(7, b"*IDN?", true);
        assert_eq!(m.len(), 12 + 8, "5 bytes of data pad to 8");
        assert_eq!(&m[..12], &[1, 7, !7, 0, 5, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(&m[12..17], b"*IDN?");
        assert_eq!(&m[17..], &[0, 0, 0]);

        let m = encode_dev_dep_msg_out(8, b"ABCD", false);
        assert_eq!(m.len(), 16, "an exact multiple needs no padding");
        assert_eq!(m[8], 0, "EOM clear");
    }

    #[test]
    fn request_in_carries_size_and_term_char() {
        let m = encode_request_dev_dep_msg_in(9, 0x1_0000, None);
        assert_eq!(m, vec![2, 9, !9, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        let m = encode_request_dev_dep_msg_in(9, 64, Some(b'\n'));
        assert_eq!(&m[8..10], &[0x02, b'\n']);
    }

    #[test]
    fn trigger_is_a_bare_header() {
        assert_eq!(
            encode_trigger(3),
            vec![128, 3, !3, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    fn in_transfer(tag: u8, data: &[u8], attrs: u8) -> Vec<u8> {
        let mut v = vec![2, tag, !tag, 0];
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&[attrs, 0, 0, 0]);
        v.extend_from_slice(data);
        v.extend(std::iter::repeat_n(0u8, pad_len(data.len())));
        v
    }

    #[test]
    fn parse_in_message_strips_header_and_padding() {
        let buf = in_transfer(5, b"HELLO", 0x01);
        let m = parse_dev_dep_msg_in(&buf, 5).unwrap();
        assert_eq!(m.data, b"HELLO");
        assert!(m.eom);
        assert!(!m.term_char_seen);
        assert_eq!(m.transfer_size, 5);
    }

    #[test]
    fn parse_in_message_reports_term_char_and_partial_transfers() {
        let buf = in_transfer(5, b"AB\n", 0x02);
        let m = parse_dev_dep_msg_in(&buf, 5).unwrap();
        assert!(m.term_char_seen && !m.eom);

        // Header promises 10 bytes, only 4 arrived: keep the 4, say so.
        let mut short = in_transfer(6, b"ABCD", 0x00);
        short[4] = 10;
        let m = parse_dev_dep_msg_in(&short, 6).unwrap();
        assert_eq!(m.data, b"ABCD");
        assert_eq!(m.transfer_size, 10);
    }

    #[test]
    fn parse_in_message_refuses_wrong_tag_and_msgid() {
        let buf = in_transfer(5, b"X", 1);
        let e = parse_dev_dep_msg_in(&buf, 6).unwrap_err().to_string();
        assert!(e.contains("out of step"), "{e}");

        let mut bad = in_transfer(5, b"X", 1);
        bad[0] = 1;
        assert!(parse_dev_dep_msg_in(&bad, 5).is_err());

        let mut bad_inverse = in_transfer(5, b"X", 1);
        bad_inverse[2] = 0;
        assert!(parse_dev_dep_msg_in(&bad_inverse, 5).is_err());

        assert!(parse_dev_dep_msg_in(&[2, 5, !5], 5).is_err());
    }

    #[test]
    fn capabilities_decode_usb488_block() {
        // status, reserved, bcdUSBTMC 1.00, iface caps, dev caps (TermChar),
        // 6 reserved, bcdUSB488 1.00, 488 iface caps (488.2 | RL | TRIGGER),
        // 488 dev caps (SCPI | SR1 | RL1 | DT1), 8 reserved.
        let mut buf = vec![1, 0, 0x00, 0x01, 0, 0x01, 0, 0, 0, 0, 0, 0];
        buf.extend_from_slice(&[0x00, 0x01, 0x07, 0x0f]);
        buf.extend_from_slice(&[0; 8]);
        let c = parse_capabilities(&buf).unwrap();
        assert_eq!(c.bcd_usbtmc, 0x0100);
        assert!(c.term_char);
        assert_eq!(c.bcd_usb488, 0x0100);
        assert!(c.ieee_488_2 && c.remote_local && c.trigger);
        assert!(c.scpi && c.sr1 && c.rl1 && c.dt1);
    }

    #[test]
    fn capabilities_without_usb488_block_advertise_nothing() {
        let buf = [1, 0, 0x00, 0x01, 0, 0x00, 0, 0, 0, 0, 0, 0];
        let c = parse_capabilities(&buf).unwrap();
        assert_eq!(c.bcd_usb488, 0);
        assert!(!c.remote_local && !c.trigger && !c.term_char);
    }

    #[test]
    fn capabilities_reject_failure_status() {
        let e = parse_capabilities(&[0x80, 0, 0, 0, 0, 0])
            .unwrap_err()
            .to_string();
        assert!(e.contains("0x80"), "{e}");
    }

    #[test]
    fn status_check_names_the_code() {
        assert!(check_status("x", &[STATUS_SUCCESS]).is_ok());
        let e = check_status("REN_CONTROL", &[STATUS_TRANSFER_NOT_IN_PROGRESS])
            .unwrap_err()
            .to_string();
        assert!(e.contains("REN_CONTROL") && e.contains("0x81"), "{e}");
        assert!(check_status("x", &[]).is_err());
    }
}
