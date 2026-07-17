// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use anyhow::{bail, Context, Result};
use nusb::transfer::{ControlOut, ControlType, Recipient};

#[derive(Debug, Clone)]
pub struct HexRecord {
    pub address: u16,
    pub data: Vec<u8>,
}

const FX2_FIRMWARE: &[u8] = include_bytes!("../../../firmware/measat_releaseX1.8.hex");

// FX2 vendor-request constants for 8051 memory access.
const ANCHOR_LOAD_INTERNAL: u8 = 0xA0;
const CPUCS_ADDR: u16 = 0xE600;

/// Upload firmware to an FX2 device in pre-init state.
/// Holds the 8051 in reset, writes all HEX records, then releases reset.
pub async fn upload_firmware(device: &nusb::Device) -> Result<()> {
    let hex_text = std::str::from_utf8(FX2_FIRMWARE).context("firmware blob is not valid UTF-8")?;
    let records = parse_hex(hex_text).context("failed to parse firmware HEX")?;

    tracing::info!("holding 8051 in reset");
    fx2_control(device, CPUCS_ADDR, &[0x01])
        .await
        .context("failed to hold 8051 in reset")?;

    tracing::info!(records = records.len(), "writing firmware records");
    for record in &records {
        fx2_control(device, record.address, &record.data)
            .await
            .with_context(|| {
                format!("failed to write firmware record at {:#06x}", record.address)
            })?;
    }

    tracing::info!("releasing 8051 from reset");
    fx2_control(device, CPUCS_ADDR, &[0x00])
        .await
        .context("failed to release 8051 from reset")?;

    Ok(())
}

async fn fx2_control(device: &nusb::Device, address: u16, data: &[u8]) -> Result<()> {
    let completion = device
        .control_out(ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: ANCHOR_LOAD_INTERNAL,
            value: address,
            index: 0,
            data,
        })
        .await;
    completion
        .into_result()
        .map_err(|e| anyhow::anyhow!("FX2 vendor control-out failed: {e}"))?;
    Ok(())
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
