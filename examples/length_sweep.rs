// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Does the 8026's first-byte loss depend on how long its response is?
//
// Firmware analysis says the instrument loads its GPIB chip's FIFO in
// ~190 us + 7.6 us per character, and that it only loses its first byte when
// that load has finished before the controller releases ATN. On the pipelined
// build the controller's addressing-to-ATN gap is a deterministic ~231 us, so
// the prediction is sharp: responses of about five characters or fewer are
// primed in time and lose their first byte, longer ones do not.
//
// This is the falsifiable test of that model. No serial poll anywhere near the
// read -- a poll primes the instrument regardless of length and would wash the
// effect out.
//
//     cargo run --example length_sweep -- [host] [port] [pad] [n] [delay_ms]

use ugpibd::vxi11::client::Vxi11Client;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let ms = |n| std::time::Duration::from_millis(n);
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.next().map_or(Ok(9010), |p| p.parse())?;
    let pad: u8 = args.next().map_or(Ok(4), |p| p.parse())?;
    let n: usize = args.next().map_or(Ok(100), |p| p.parse())?;
    let delay: u64 = args.next().map_or(Ok(200), |p| p.parse())?;

    let mut c = Vxi11Client::connect(&host, port).await?;
    let link = c.create_link(&format!("gpib0,{pad}")).await?;
    anyhow::ensure!(link.error == 0, "link: error {}", link.error);
    let lid = link.lid;

    // Stable queries only: *ESR? is destructive (reading clears it) and its
    // answer would change under us.
    // Expected answers are pinned, not probed. At a 100% loss rate every probe
    // comes back empty and a probed reference would define the truncated answer
    // as correct, reporting a clean run on a completely broken one. None means
    // "probe it" and is only safe for answers too long to vanish entirely.
    let queries: [(&str, Option<&str>); 3] =
        [("*OPC?", Some("1")), ("*SRE?", Some("0")), ("*IDN?", None)];
    println!("{n} reads per query, {delay} ms query-to-read delay, no polls\n");
    println!("  query      response                     len   first byte lost   other");
    for (q, expected) in queries {
        // Reference answer. A single probe read is not safe here: the very
        // effect being measured can eat the probe's first byte, and a
        // truncated reference silently inverts the whole test (it did, on the
        // first run of this). Loss only ever removes the leading byte, so take
        // the longest of several probes.
        let mut want = expected.unwrap_or_default().to_string();
        for _ in 0..if expected.is_some() { 0 } else { 5 } {
            let w = c
                .device_write(lid, format!("{q}\n").as_bytes(), true, 5000)
                .await?;
            anyhow::ensure!(w.error == 0, "write {q}: error {}", w.error);
            tokio::time::sleep(ms(delay)).await;
            let r = c.device_read(lid, 512, 5000, None).await?;
            let got = String::from_utf8_lossy(&r.data).trim().to_string();
            if got.len() > want.len() {
                want = got;
            }
        }

        let mut short = 0usize;
        let mut other = 0usize;
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..n {
            let w = c
                .device_write(lid, format!("{q}\n").as_bytes(), true, 5000)
                .await?;
            anyhow::ensure!(w.error == 0, "write {q}: error {}", w.error);
            tokio::time::sleep(ms(delay)).await;
            let r = c.device_read(lid, 512, 5000, None).await?;
            let got = String::from_utf8_lossy(&r.data).trim().to_string();
            if got == want {
                continue;
            }
            if !got.is_empty() && want.ends_with(&got) && got.len() == want.len() - 1 {
                short += 1;
            } else if got.is_empty() && want.len() == 1 {
                // A one-character response that lost its only byte.
                short += 1;
            } else {
                other += 1;
                if seen.len() < 6 && !seen.contains(&got) {
                    seen.push(got);
                }
            }
        }
        println!(
            "  {q:<9}  {:<28}  {:>3}   {short:>4}/{n} ({:>5.1}%)   {other}",
            format!("{want:?}"),
            want.len(),
            100.0 * short as f64 / n as f64
        );
        if !seen.is_empty() {
            println!("             other responses seen: {seen:?}");
        }
    }
    Ok(())
}
