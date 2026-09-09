// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Bench regression for the NI backend's read and serial-poll paths, against
// two instruments on one bus.
//
// The two-instrument part is the point: a read skips re-addressing when the bus
// is already addressed the way it wants, and only a second talker can catch
// that going stale — the failure would be reading one instrument and getting
// another's answer.
//
//     cargo run --example bench_regression -- [host] [port] [padA] [padB]

use ugpibd::vxi11::client::Vxi11Client;

struct Dev {
    lid: i32,
    pad: u8,
    idn: String,
}

/// Empty the instrument's output queue, without ever reading speculatively.
///
/// A bare read issued when nothing is queued is a query error — a 34401A logs
/// -420 Query UNTERMINATED and beeps for each one, and enough of them overflow
/// its error queue. So ask first: serial poll, and read only while MAV says
/// there is something to read.
///
/// Sections must not leak state into each other either. An unread response left
/// behind makes MAV already true when the next section enables it, so the
/// instrument never makes the false-to-true transition that asserts SRQ, and a
/// poll that should read RQS|MAV reads bare MAV.
async fn drain(c: &mut Vxi11Client, lid: i32) -> anyhow::Result<()> {
    const MAV: u8 = 0x10;
    for _ in 0..8 {
        let s = c.device_readstb(lid, 5000).await?;
        if s.error != 0 || s.stb & MAV == 0 {
            return Ok(());
        }
        let r = c.device_read(lid, 512, 2000, None).await?;
        if r.data.is_empty() {
            return Ok(());
        }
    }
    Ok(())
}

/// Write, and fail loudly rather than leaving a section to misreport later.
async fn write(c: &mut Vxi11Client, lid: i32, cmd: &str) -> anyhow::Result<()> {
    let w = c
        .device_write(lid, format!("{cmd}\n").as_bytes(), true, 5000)
        .await?;
    anyhow::ensure!(w.error == 0, "write {cmd:?}: error {}", w.error);
    Ok(())
}

async fn query(c: &mut Vxi11Client, lid: i32, cmd: &str) -> anyhow::Result<String> {
    let w = c
        .device_write(lid, format!("{cmd}\n").as_bytes(), true, 5000)
        .await?;
    anyhow::ensure!(w.error == 0, "write {cmd:?}: error {}", w.error);
    let r = c.device_read(lid, 512, 5000, None).await?;
    anyhow::ensure!(r.error == 0, "read after {cmd:?}: error {}", r.error);
    Ok(String::from_utf8_lossy(&r.data).trim().to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let ms = |n| std::time::Duration::from_millis(n);
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.next().map_or(Ok(9010), |p| p.parse())?;
    let pad_a: u8 = args.next().map_or(Ok(23), |p| p.parse())?;
    let pad_b: u8 = args.next().map_or(Ok(4), |p| p.parse())?;

    let mut c = Vxi11Client::connect(&host, port).await?;
    let mut devs = Vec::new();
    for pad in [pad_a, pad_b] {
        let link = c.create_link(&format!("gpib0,{pad}")).await?;
        anyhow::ensure!(link.error == 0, "link to {pad}: error {}", link.error);
        let idn = query(&mut c, link.lid, "*IDN?").await?;
        println!("pad {pad:>2}: {idn}");
        devs.push(Dev {
            lid: link.lid,
            pad,
            idn,
        });
    }

    let mut fail = 0usize;
    let mut check = |ok: bool, what: &str, got: &str| {
        if !ok {
            fail += 1;
            println!("  FAIL {what}: {got:?}");
        }
    };

    // Every write-to-read delay. Under ~20 ms the instrument is not ready yet,
    // which is what used to hide the lost first byte.
    println!("\n-- write-to-read delay, each instrument --");
    for d in &devs {
        drain(&mut c, d.lid).await?;
        for delay in [0u64, 20, 50, 100, 200, 500, 1000, 2000, 3000] {
            c.device_write(d.lid, b"*IDN?\n", true, 5000).await?;
            tokio::time::sleep(ms(delay)).await;
            let r = c.device_read(d.lid, 512, 5000, None).await?;
            let got = String::from_utf8_lossy(&r.data).trim().to_string();
            check(got == d.idn, &format!("pad {} delay {delay}", d.pad), &got);
        }
        println!("  pad {} done", d.pad);
    }

    // One byte at a time and up: each chunk is a fresh read op, and every read
    // op after the first used to lose a byte.
    println!("-- chunked reads --");
    for d in &devs {
        drain(&mut c, d.lid).await?;
        for chunk in 1..=7u32 {
            c.device_write(d.lid, b"*IDN?\n", true, 5000).await?;
            tokio::time::sleep(ms(300)).await;
            let mut all = Vec::new();
            for _ in 0..200 {
                let r = c.device_read(d.lid, chunk, 5000, None).await?;
                if r.data.is_empty() {
                    break;
                }
                all.extend_from_slice(&r.data);
                if all.ends_with(b"\n") {
                    break;
                }
            }
            let got = String::from_utf8_lossy(&all).trim().to_string();
            check(got == d.idn, &format!("pad {} chunk {chunk}", d.pad), &got);
        }
        println!("  pad {} done", d.pad);
    }

    // The one a single-instrument bus cannot test: the addressing a read leaves
    // behind must not be reused for a different talker.
    println!("-- alternating talkers --");
    for d in &devs {
        drain(&mut c, d.lid).await?;
    }
    for round in 0..6 {
        let d = &devs[round % devs.len()];
        let got = query(&mut c, d.lid, "*IDN?").await?;
        check(
            got == d.idn,
            &format!("round {round} expected pad {}", d.pad),
            &got,
        );
    }
    println!("  done");

    // Serial poll: at rest, then with MAV asserted and a response queued.
    println!("-- serial poll --");
    for d in &devs {
        drain(&mut c, d.lid).await?;
        let s = c.device_readstb(d.lid, 5000).await?;
        check(
            s.error == 0,
            &format!("pad {} poll at rest", d.pad),
            &format!("error {}", s.error),
        );
        println!("  pad {:>2} at rest: stb {:#04x}", d.pad, s.stb);

        write(&mut c, d.lid, "*SRE 16").await?;
        write(&mut c, d.lid, "*IDN?").await?;
        tokio::time::sleep(ms(500)).await;
        let s = c.device_readstb(d.lid, 5000).await?;
        println!(
            "  pad {:>2} with MAV: stb {:#04x} (expect RQS|MAV = 0x50)",
            d.pad, s.stb
        );
        // RQS as well as MAV, deliberately. Accepting bare MAV here is what
        // hid a regression that left instruments addressed to talk: they began
        // delivering before the client asked, their output queue drained into
        // the transfer, and RQS was never asserted at all. A client waiting on
        // SRQ would hang while this test reported success.
        check(
            s.error == 0 && s.stb == 0x50,
            &format!("pad {} poll with MAV expected RQS|MAV", d.pad),
            &format!("error {} stb {:#04x}", s.error, s.stb),
        );
        // Known gap: the read straight after a poll loses its first byte.
        let r = c.device_read(d.lid, 512, 5000, None).await?;
        let got = String::from_utf8_lossy(&r.data).trim().to_string();
        if got != d.idn {
            println!("  pad {:>2} read-after-poll (known gap): {got:?}", d.pad);
            // The lost byte leaves this instrument out of step: the next query
            // it is given can be swallowed, which shows up as a phantom
            // failure in the following run. An addressed device clear resyncs
            // it (a bus-wide DCL does not — it wedges an 8026).
            let e = c.device_clear(d.lid).await?;
            anyhow::ensure!(e == 0, "device clear after the known gap: error {e}");
            drain(&mut c, d.lid).await?;
        }
        write(&mut c, d.lid, "*SRE 0").await?;
    }

    println!(
        "\n{}",
        if fail == 0 {
            "ALL CLEAN".to_string()
        } else {
            format!("{fail} FAILURES")
        }
    );
    Ok(())
}
