// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// How often does a read straight after a serial poll lose its first byte?
//
// The 8026 starts loading its GPIB chip's FIFO when "addressed to talk AND not
// in serial-poll mode AND a response pending" all hold. A poll teardown of
// SPD-then-UNT opens that window for one command byte, about 8 us; if the
// instrument's ~68 us busy-poll lands inside it, the FIFO is loaded and the
// next read's ATN edge finds it primed, which costs the first byte.
//
// Captures put the catch rate at 4 of 9, where the window arithmetic predicts
// about 12%. Nine samples cannot tell those apart, hence this: run hundreds.
//
// It is also the verification for the UNT-before-SPD change. Run it on both
// branches against the same instrument:
//
//     cargo run --example poll_then_read -- [host] [port] [pad] [n] [delay_ms]
//
// --no-poll style control: pass a delay and pad but n=0 to skip. The control
// arm (query then read, no poll) runs automatically and should show the
// background rate, which is much lower.

use ugpibd::vxi11::client::Vxi11Client;

const MAV: u8 = 0x10;

async fn write(c: &mut Vxi11Client, lid: i32, cmd: &str) -> anyhow::Result<()> {
    let w = c
        .device_write(lid, format!("{cmd}\n").as_bytes(), true, 5000)
        .await?;
    anyhow::ensure!(w.error == 0, "write {cmd:?}: error {}", w.error);
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let ms = |n| std::time::Duration::from_millis(n);
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.next().map_or(Ok(9010), |p| p.parse())?;
    let pad: u8 = args.next().map_or(Ok(4), |p| p.parse())?;
    let n: usize = args.next().map_or(Ok(200), |p| p.parse())?;
    let delay: u64 = args.next().map_or(Ok(200), |p| p.parse())?;

    let mut c = Vxi11Client::connect(&host, port).await?;
    let link = c.create_link(&format!("gpib0,{pad}")).await?;
    anyhow::ensure!(link.error == 0, "link: error {}", link.error);
    let lid = link.lid;

    // Reference response, taken without a poll anywhere near it.
    write(&mut c, lid, "*IDN?").await?;
    tokio::time::sleep(ms(delay)).await;
    let r = c.device_read(lid, 512, 5000, None).await?;
    let idn = String::from_utf8_lossy(&r.data).trim().to_string();
    println!("pad {pad}: {idn:?} ({} bytes)", idn.len());

    for (name, poll) in [("poll then read", true), ("control: read only", false)] {
        let mut short = 0usize;
        let mut other = 0usize;
        let mut stb_bad = 0usize;
        for _ in 0..n {
            write(&mut c, lid, "*IDN?").await?;
            tokio::time::sleep(ms(delay)).await;
            if poll {
                let s = c.device_readstb(lid, 5000).await?;
                if s.error != 0 || s.stb & MAV == 0 {
                    stb_bad += 1;
                }
            }
            let r = c.device_read(lid, 512, 5000, None).await?;
            let got = String::from_utf8_lossy(&r.data).trim().to_string();
            if got == idn {
                continue;
            }
            // A lost first byte is the tail of the expected string.
            if !got.is_empty() && idn.ends_with(&got) && got.len() == idn.len() - 1 {
                short += 1;
            } else {
                other += 1;
            }
        }
        println!(
            "  {name:<20} first byte lost {short}/{n} ({:.1}%)   other mismatches {other}   \
             polls without MAV {stb_bad}",
            100.0 * short as f64 / n as f64
        );
    }
    Ok(())
}
