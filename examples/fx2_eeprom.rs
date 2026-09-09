// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Read the I2C EEPROM out of an NI GPIB-USB-HS (Cypress FX2LP).
//
// The adapter's own firmware exposes no memory read — vendor request 0xA0
// returns a sample of the 8051's instruction fetch bus, not RAM. So this works
// on a part that has been prevented from booting its EEPROM: hold SDA (or SCL)
// low while plugging in and the FX2 falls back to its default identity
// 04b4:8613, where RENUM=0 leaves vendor request 0xA0 (RAM read/write) to the
// USB core in hardware. No firmware of NI's is running, and none of ours needs
// to speak USB.
//
//     hold the pin, plug in, release, then:
//     cargo run --example fx2_eeprom -- eeprom.bin
//
// This only ever *reads* the EEPROM: no I2C write is issued anywhere, so the
// adapter's firmware cannot be damaged. The 8051 is left in reset at the end;
// unplug and replug to get the adapter back.

use anyhow::{bail, Context, Result};
use nusb::MaybeFuture;
use std::time::Duration;

/// I2C EEPROM reader for the FX2LP, built from eeprom.c with sdcc.
/// Loads at 0x0000; buffer at 0x1000; control bytes at 0x0FFA..0x0FFD.
const FIRMWARE: [u8; 459] = [
    0x02, 0x00, 0x4e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xe2, 0xfb, 0xea, 0xf2, 0x80, 0x2c,
    0x00, 0x00, 0xe0, 0xfb, 0xea, 0xf0, 0x80, 0x24, 0xe6, 0xb5, 0x02, 0x02, 0xeb, 0xf6, 0x22, 0x00,
    0xe2, 0xb5, 0x02, 0x02, 0xeb, 0xf2, 0x22, 0x00, 0xe0, 0xb5, 0x02, 0x02, 0xeb, 0xf0, 0x22, 0x30,
    0xf6, 0xe0, 0xa8, 0x82, 0x20, 0xf5, 0xd3, 0xea, 0xc6, 0xf5, 0x82, 0x22, 0x8b, 0x82, 0x22, 0x30,
    0xf6, 0xe6, 0xa8, 0x82, 0x20, 0xf5, 0xd9, 0x80, 0xcf, 0x12, 0x00, 0xcb, 0x80, 0xfe, 0x75, 0x81,
    0x07, 0x12, 0x01, 0xc7, 0xe5, 0x82, 0x60, 0x03, 0x02, 0x00, 0x49, 0x79, 0x00, 0xe9, 0x44, 0x00,
    0x60, 0x1b, 0x7a, 0x00, 0x90, 0x01, 0xcb, 0x78, 0x00, 0x75, 0xa0, 0x30, 0xe4, 0x93, 0xf2, 0xa3,
    0x08, 0xb8, 0x00, 0x02, 0x05, 0xa0, 0xd9, 0xf4, 0xda, 0xf2, 0x75, 0xa0, 0xff, 0xe4, 0x78, 0xff,
    0xf6, 0xd8, 0xfd, 0x78, 0x00, 0xe8, 0x44, 0x00, 0x60, 0x0a, 0x79, 0x00, 0x75, 0xa0, 0x30, 0xe4,
    0xf3, 0x09, 0xd8, 0xfc, 0x78, 0x00, 0xe8, 0x44, 0x00, 0x60, 0x0c, 0x79, 0x00, 0x90, 0x30, 0x00,
    0xe4, 0xf0, 0xa3, 0xd8, 0xfc, 0xd9, 0xfa, 0x02, 0x00, 0x49, 0x7e, 0x00, 0x7f, 0x00, 0x90, 0xe6,
    0x78, 0xe0, 0x20, 0xe0, 0x12, 0x74, 0x01, 0x2e, 0xfc, 0xe4, 0x3f, 0xfd, 0x8c, 0x06, 0x8d, 0x07,
    0xec, 0x4d, 0x70, 0xea, 0xf5, 0x82, 0x22, 0x75, 0x82, 0x01, 0x22, 0x90, 0x0f, 0xfb, 0xe4, 0xf0,
    0x90, 0xe6, 0x78, 0x74, 0x80, 0xf0, 0x90, 0x0f, 0xfa, 0xe0, 0x90, 0xe6, 0x79, 0xf0, 0x12, 0x00,
    0xaa, 0xe5, 0x82, 0x70, 0x09, 0x90, 0x0f, 0xfb, 0x74, 0xe3, 0xf0, 0x02, 0x01, 0xbe, 0x90, 0xe6,
    0x78, 0xe0, 0x20, 0xe1, 0x09, 0x90, 0x0f, 0xfb, 0x74, 0xe1, 0xf0, 0x02, 0x01, 0xbe, 0x90, 0x0f,
    0xfc, 0xe0, 0x90, 0xe6, 0x79, 0xf0, 0x12, 0x00, 0xaa, 0xe5, 0x82, 0x70, 0x09, 0x90, 0x0f, 0xfb,
    0x74, 0xe3, 0xf0, 0x02, 0x01, 0xbe, 0x90, 0x0f, 0xfd, 0xe0, 0x90, 0xe6, 0x79, 0xf0, 0x12, 0x00,
    0xaa, 0xe5, 0x82, 0x70, 0x09, 0x90, 0x0f, 0xfb, 0x74, 0xe3, 0xf0, 0x02, 0x01, 0xbe, 0x90, 0xe6,
    0x78, 0x74, 0x80, 0xf0, 0x90, 0x0f, 0xfa, 0xe0, 0x44, 0x01, 0x90, 0xe6, 0x79, 0xf0, 0x12, 0x00,
    0xaa, 0xe5, 0x82, 0x70, 0x08, 0x90, 0x0f, 0xfb, 0x74, 0xe3, 0xf0, 0x80, 0x71, 0x90, 0xe6, 0x78,
    0xe0, 0x20, 0xe1, 0x08, 0x90, 0x0f, 0xfb, 0x74, 0xe2, 0xf0, 0x80, 0x62, 0x90, 0xe6, 0x79, 0xe0,
    0x12, 0x00, 0xaa, 0xe5, 0x82, 0x70, 0x08, 0x90, 0x0f, 0xfb, 0x74, 0xe3, 0xf0, 0x80, 0x4f, 0x7e,
    0x00, 0x7f, 0x00, 0x8e, 0x04, 0x8f, 0x05, 0xbc, 0xff, 0x0a, 0xbd, 0x1f, 0x07, 0x90, 0xe6, 0x78,
    0xe0, 0x44, 0x20, 0xf0, 0x8e, 0x04, 0x74, 0x10, 0x2f, 0xfd, 0x90, 0xe6, 0x79, 0xe0, 0x8c, 0x82,
    0x8d, 0x83, 0xf0, 0xc0, 0x07, 0xc0, 0x06, 0x12, 0x00, 0xaa, 0xe5, 0x82, 0xd0, 0x06, 0xd0, 0x07,
    0x70, 0x08, 0x90, 0x0f, 0xfb, 0x74, 0xe3, 0xf0, 0x80, 0x14, 0x0e, 0xbe, 0x00, 0x01, 0x0f, 0x8e,
    0x04, 0x8f, 0x05, 0x74, 0xe0, 0x2d, 0x50, 0xbb, 0x90, 0x0f, 0xfb, 0x74, 0xaa, 0xf0, 0x90, 0xe6,
    0x78, 0xe0, 0x44, 0x40, 0xf0, 0x80, 0xfe, 0x75, 0x82, 0x00, 0x22,
];

const VID_CYPRESS: u16 = 0x04b4;
const PID_FX2_DEFAULT: u16 = 0x8613;
const VID_NI: u16 = 0x3923;

const REQ_FIRMWARE: u8 = 0xA0;
const REG_CPUCS: u16 = 0xE600;

const ADDR_DADDR: u16 = 0x0FFA;
const ADDR_STATUS: u16 = 0x0FFB;
const ADDR_HI: u16 = 0x0FFC;
const ADDR_BUF: u16 = 0x1000;
const CHUNK: usize = 0x2000;
const EEPROM_LEN: usize = 0x8000;

async fn ram_write(dev: &nusb::Device, addr: u16, data: &[u8]) -> Result<()> {
    for (i, part) in data.chunks(64).enumerate() {
        dev.control_out(
            nusb::transfer::ControlOut {
                control_type: nusb::transfer::ControlType::Vendor,
                recipient: nusb::transfer::Recipient::Device,
                request: REQ_FIRMWARE,
                value: addr + (i * 64) as u16,
                index: 0,
                data: part,
            },
            Duration::from_secs(2),
        )
        .await
        .with_context(|| format!("RAM write at {:#06x}", addr as usize + i * 64))?;
    }
    Ok(())
}

async fn ram_read(dev: &nusb::Device, addr: u16, len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let want = (len - out.len()).min(64);
        let b = dev
            .control_in(
                nusb::transfer::ControlIn {
                    control_type: nusb::transfer::ControlType::Vendor,
                    recipient: nusb::transfer::Recipient::Device,
                    request: REQ_FIRMWARE,
                    value: addr + out.len() as u16,
                    index: 0,
                    length: want as u16,
                },
                Duration::from_secs(2),
            )
            .await
            .with_context(|| format!("RAM read at {:#06x}", addr as usize + out.len()))?;
        if b.is_empty() {
            bail!("short RAM read at {:#06x}", addr as usize + out.len());
        }
        out.extend_from_slice(&b);
    }
    Ok(out)
}

/// true = held in reset.
async fn cpu_reset(dev: &nusb::Device, held: bool) -> Result<()> {
    ram_write(dev, REG_CPUCS, &[u8::from(held)]).await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "eeprom.bin".into());

    let devs: Vec<_> = nusb::list_devices().wait()?.collect();
    if let Some(d) = devs.iter().find(|d| d.vendor_id() == VID_NI) {
        bail!(
            "found the adapter running its own firmware ({:04x}:{:04x}) — the EEPROM was read at \
             boot, so the pin was not held. Hold SDA low, plug in, then release.",
            d.vendor_id(),
            d.product_id()
        );
    }
    let info = devs
        .iter()
        .find(|d| d.vendor_id() == VID_CYPRESS && d.product_id() == PID_FX2_DEFAULT)
        .context("no FX2 in default mode (04b4:8613) and no NI adapter found")?;
    println!(
        "found {:04x}:{:04x} — FX2 in default mode",
        info.vendor_id(),
        info.product_id()
    );
    let dev = info.open().wait().context("open device")?;

    cpu_reset(&dev, true).await.context("hold 8051 in reset")?;
    ram_write(&dev, 0x0000, &FIRMWARE)
        .await
        .context("load firmware")?;
    let back = ram_read(&dev, 0x0000, FIRMWARE.len()).await?;
    if back != FIRMWARE {
        bail!("firmware read back differently than written; RAM access is not working");
    }
    println!("loaded {} bytes of firmware, verified", FIRMWARE.len());

    // Do not assume 0xA0. The board may strap A0..A2, and if you defeated the
    // boot by strapping one of them high, the EEPROM is answering somewhere
    // else right now. Probe before committing to an address.
    let mut daddr = None;
    for cand in [0xA0u8, 0xA2, 0xA4, 0xA6, 0xA8, 0xAA, 0xAC, 0xAE] {
        cpu_reset(&dev, true).await?;
        ram_write(&dev, ADDR_DADDR, &[cand]).await?;
        ram_write(&dev, ADDR_STATUS, &[0x00]).await?;
        ram_write(&dev, ADDR_HI, &[0x00, 0x00]).await?;
        cpu_reset(&dev, false).await?;
        let mut st = 0u8;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            st = ram_read(&dev, ADDR_STATUS, 1).await?[0];
            if st != 0 {
                break;
            }
        }
        cpu_reset(&dev, true).await?;
        if st == 0xAA {
            let head = ram_read(&dev, ADDR_BUF, 1).await?[0];
            println!("  {cand:#04x}: answered, first byte {head:#04x}");
            daddr = Some(cand);
            break;
        }
        println!("  {cand:#04x}: no answer (status {st:#04x})");
    }
    let daddr = daddr.context(
        "no EEPROM answered on any address 0xA0..0xAE — if you are still holding a pin to          defeat the boot, release it now and re-run; the read needs the bus free",
    )?;
    println!("using device address {daddr:#04x}");

    let mut image = Vec::with_capacity(EEPROM_LEN);
    while image.len() < EEPROM_LEN {
        let base = image.len() as u16;
        cpu_reset(&dev, true).await?;
        ram_write(&dev, ADDR_DADDR, &[daddr]).await?;
        ram_write(&dev, ADDR_STATUS, &[0x00]).await?;
        ram_write(&dev, ADDR_HI, &[(base >> 8) as u8, (base & 0xff) as u8]).await?;
        cpu_reset(&dev, false).await?;

        let mut status = 0u8;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            status = ram_read(&dev, ADDR_STATUS, 1).await?[0];
            if status != 0 {
                break;
            }
        }
        cpu_reset(&dev, true).await?;
        match status {
            0xAA => {}
            0x00 => bail!("firmware did not finish reading at {base:#06x} (no status)"),
            0xE1 => bail!("EEPROM did not acknowledge its address — is SDA released now?"),
            0xE2 => bail!("EEPROM did not acknowledge the read command"),
            0xE3 => bail!("I2C bus error at {base:#06x}"),
            s => bail!("unexpected firmware status {s:#04x}"),
        }
        image.extend_from_slice(&ram_read(&dev, ADDR_BUF, CHUNK).await?);
        println!("  read {:#06x}..{:#06x}", base, image.len());
    }

    std::fs::write(&out, &image)?;
    println!("wrote {} bytes to {out}", image.len());
    if image.first() == Some(&0xC2) {
        println!("starts with 0xC2: a valid FX2 boot EEPROM");
    } else {
        println!("warning: first byte is {:#04x}, expected 0xC2", image[0]);
    }
    println!("the 8051 is left in reset — unplug and replug to restore the adapter");
    Ok(())
}
