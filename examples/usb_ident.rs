// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Dump the USB identity of a connected GPIB adapter: descriptors, strings,
// endpoints, and the NI vendor requests that report serial and version.
//
// Useful for comparing one adapter against another, or against a known-good
// unit — descriptor details are where re-implementations tend to diverge.
//
//     cargo run --example usb_ident

use anyhow::{Context, Result};
use nusb::MaybeFuture;
use std::time::Duration;

const VID_NI: u16 = 0x3923;
const VID_AGILENT: u16 = 0x0957;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let devs: Vec<_> = nusb::list_devices()
        .wait()?
        .filter(|d| d.vendor_id() == VID_NI || d.vendor_id() == VID_AGILENT)
        .collect();
    if devs.is_empty() {
        anyhow::bail!("no NI or Agilent GPIB adapter found");
    }

    for info in &devs {
        println!("=== {:04x}:{:04x} ===", info.vendor_id(), info.product_id());
        println!("  manufacturer : {:?}", info.manufacturer_string());
        println!("  product      : {:?}", info.product_string());
        println!("  serial       : {:?}", info.serial_number());

        let dev = info.open().wait().context("open")?;
        let cfg = dev.active_configuration().context("active configuration")?;
        println!(
            "  configuration {} ({:?}), {} interface(s)",
            cfg.configuration_value(),
            cfg.string_index(),
            cfg.interfaces().count()
        );
        for iface in cfg.interfaces() {
            for alt in iface.alt_settings() {
                println!(
                    "    interface {} alt {}: class {:#04x}/{:#04x}/{:#04x}, {} endpoint(s)",
                    alt.interface_number(),
                    alt.alternate_setting(),
                    alt.class(),
                    alt.subclass(),
                    alt.protocol(),
                    alt.endpoints().count()
                );
                for ep in alt.endpoints() {
                    println!(
                        "      ep {:#04x} {:?} {:?} max {}",
                        ep.address(),
                        ep.direction(),
                        ep.transfer_type(),
                        ep.max_packet_size()
                    );
                }
            }
        }

        // NI vendor reads that carry identity. Read-only; a device that does
        // not implement one simply stalls.
        //
        // 0x40 is the one that matters: on a GPIB-USB-HS its reply carries the
        // firmware version block stored in the EEPROM between the two firmware
        // images — bytes 6..10 of the reply are that block's version field and
        // checksum. 0x48/0x4b are what the HS+ init path reads.
        for (req, what) in [
            (0x41u8, "serial number"),
            (0x40, "poll ready/ver"),
            (0x48, "hs+ init 0x48"),
            (0x4b, "hs+ init 0x4b"),
        ] {
            match dev
                .control_in(
                    nusb::transfer::ControlIn {
                        control_type: nusb::transfer::ControlType::Vendor,
                        recipient: nusb::transfer::Recipient::Device,
                        request: req,
                        value: 0,
                        index: 0,
                        length: 16,
                    },
                    Duration::from_secs(1),
                )
                .await
            {
                Ok(b) => println!("  vendor {req:#04x} ({what:<13}): {:02x?}", b),
                Err(e) => println!("  vendor {req:#04x} ({what:<13}): {e}"),
            }
        }
        // Does any request we already issue vary with wValue? A memory read
        // must; a fixed status reply will not. Only requests this driver
        // already sends in normal operation are tried here.
        println!("  -- address sensitivity of known requests --");
        for req in [0x40u8, 0x41, 0x48, 0x4b] {
            let mut replies = Vec::new();
            for v in [0x0000u16, 0x1000, 0x8000] {
                let r = dev
                    .control_in(
                        nusb::transfer::ControlIn {
                            control_type: nusb::transfer::ControlType::Vendor,
                            recipient: nusb::transfer::Recipient::Device,
                            request: req,
                            value: v,
                            index: 0,
                            length: 16,
                        },
                        Duration::from_secs(1),
                    )
                    .await
                    .unwrap_or_default();
                replies.push(r);
            }
            let varies = replies.windows(2).any(|w| w[0] != w[1]);
            println!(
                "    {req:#04x}: {}",
                if varies {
                    format!("VARIES {:02x?}", replies)
                } else {
                    "constant".to_string()
                }
            );
        }
        println!();
    }
    Ok(())
}
