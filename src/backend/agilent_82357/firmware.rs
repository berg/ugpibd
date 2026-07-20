// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use anyhow::{bail, Context, Result};
use nusb::transfer::{ControlOut, ControlType, Recipient};

#[derive(Debug, Clone)]
pub struct HexRecord {
    pub address: u16,
    pub data: Vec<u8>,
}

/// The 82357B firmware blob (Cypress FX2 part).
pub const FIRMWARE_82357B: &[u8] = include_bytes!("../../../firmware/measat_releaseX1.8.hex");
/// The 82357A firmware blob (first-generation EZ-USB / AN2131 part).
pub const FIRMWARE_82357A: &[u8] = include_bytes!("../../../firmware/82357a_fw.hex");

// EZ-USB vendor request that writes on-chip RAM while the 8051 is held in reset.
const ANCHOR_LOAD_INTERNAL: u8 = 0xA0;

/// Upload `firmware` (Intel HEX text) to an EZ-USB device in pre-init state.
/// Holds the 8051 in reset, writes all HEX records, then releases reset.
///
/// `cpucs_addr` is the CPU control/status register that resets the 8051:
/// `0xE600` on the FX2 (82357B), `0x7F92` on the first-gen EZ-USB / AN2131
/// (82357A). Everything else is identical across the two parts.
pub async fn upload_firmware(
    device: &nusb::Device,
    firmware: &[u8],
    cpucs_addr: u16,
) -> Result<()> {
    let hex_text = std::str::from_utf8(firmware).context("firmware blob is not valid UTF-8")?;
    let records = parse_hex(hex_text).context("failed to parse firmware HEX")?;

    tracing::info!("holding 8051 in reset");
    anchor_load(device, cpucs_addr, &[0x01])
        .await
        .context("failed to hold 8051 in reset")?;

    tracing::info!(records = records.len(), "writing firmware records");
    for record in &records {
        anchor_load(device, record.address, &record.data)
            .await
            .with_context(|| {
                format!("failed to write firmware record at {:#06x}", record.address)
            })?;
    }

    tracing::info!("releasing 8051 from reset");
    anchor_load(device, cpucs_addr, &[0x00])
        .await
        .context("failed to release 8051 from reset")?;

    Ok(())
}

async fn anchor_load(device: &nusb::Device, address: u16, data: &[u8]) -> Result<()> {
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
