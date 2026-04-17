// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub struct HexRecord {
    pub address: u16,
    pub data: Vec<u8>,
}

/// Parse Intel HEX text into data records. EOF record is consumed and not returned.
/// Record types other than 0x00 (data) and 0x01 (EOF) are silently skipped.
pub fn parse_hex(text: &str) -> Result<Vec<HexRecord>> {
    let mut records = Vec::new();
    for (line_num, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(':') {
            bail!("line {}: missing leading ':'", line_num + 1);
        }
        let bytes = hex_decode(&line[1..])
            .with_context(|| format!("line {}: invalid hex", line_num + 1))?;
        if bytes.len() < 5 {
            bail!("line {}: too short", line_num + 1);
        }
        let byte_count = bytes[0] as usize;
        if bytes.len() != byte_count + 5 {
            bail!("line {}: length mismatch", line_num + 1);
        }
        let sum: u8 = bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        if sum != 0 {
            bail!("line {}: bad checksum (got {sum:#x})", line_num + 1);
        }
        let address = u16::from_be_bytes([bytes[1], bytes[2]]);
        let record_type = bytes[3];
        match record_type {
            0x00 => {
                records.push(HexRecord {
                    address,
                    data: bytes[4..4 + byte_count].to_vec(),
                });
            }
            0x01 => return Ok(records),
            _ => {}
        }
    }
    Ok(records)
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("odd hex length");
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(Into::into))
        .collect()
}
