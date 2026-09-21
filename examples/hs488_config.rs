// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Try to cure a Tabor 8026's first-byte loss from the CONTROLLER side, by
// configuring the instrument rather than working around it.
//
// The 8026 loses its first byte because it sources it within 100-200 ns of ATN
// falling, with no data settling time, into a controller chip that is briefly
// ready but has no read op armed. Firmware analysis says the instrument's ROM
// asks for the longest T1 there is, so nothing in its configuration explains
// this -- but it does implement the HS488 configuration commands, and the
// documented HS488 start-up sequence changes the shape of the problem:
//
//   with HSE set, a talker that finds RFD true after ATN goes false first
//   asserts NRFD ITSELF for T17 (the HSC message, 500 ns as this ROM programs
//   it), releases it, and only then sources byte 1 with the ordinary 3-wire
//   handshake.
//
// If the part honours that, there is no longer an instant at the ATN edge when
// a byte can be handshaken into an unarmed controller: by the time the
// instrument lets NRFD go, our own chip is holding it low and the talker must
// wait for the read op to arm. Two command bytes, no firmware patch.
//
// CFE (0x1F) then CFG15 (0x6F): n=15 gives the longest counters the handler
// can program (T13 275 ns, T12 325 ns) and leaves the DAV deglitch alone.
// Never send 0x60 -- n=0 indexes off the end of the firmware's table.
//
// Everything here is undone by a power cycle; nothing is written to the
// instrument's non-volatile storage. Send it only when the instrument is idle,
// with nothing queued: the 8026's command handler runs with interrupts off
// while a transfer is in flight, and a command handshake into that state hangs
// the bus until power is removed.
//
// The configuration byte is hard-coded to CFG15 (0x6F): n=15 gives the longest
// counters the handler can program and leaves the DAV deglitch alone, and n=0
// would index off the end of the firmware's table. There is no reason to vary
// it, so it is not exposed.
//
// HS488 state is invisible -- nothing in the instrument reports it, the
// register is write-only and the firmware never reads it back -- so the only
// way to know it took is behaviour. This checks afterwards, with a
// one-character query whose answer is pinned here rather than probed, because
// that query fails 100% of the time on an unconfigured instrument.
//
//     cargo run --example hs488_config -- [host] [port] [pad]

use ugpibd::vxi11::client::Vxi11Client;

const UNL: u8 = 0x3f;
const CFE: u8 = 0x1f;
/// CFG15. n=15 programs the longest counters the firmware's table holds and
/// leaves the DAV deglitch at its initialised value; n=0 would index off the
/// end of that table. Not exposed, because there is no reason to vary it.
const CFG15: u8 = 0x6f;

fn mla(pad: u8) -> u8 {
    0x20 | pad
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.next().map_or(Ok(9010), |p| p.parse())?;
    let pad: u8 = args.next().map_or(Ok(4), |p| p.parse())?;
    let n: u8 = args.next().map_or(Ok(15), |p| p.parse())?;
    anyhow::ensure!(
        (1..=15).contains(&n),
        "n must be 1-15; 0 reads off the end of the firmware's table"
    );

    let mut c = Vxi11Client::connect(&host, port).await?;
    let dev = c.create_link(&format!("gpib0,{pad}")).await?;
    anyhow::ensure!(dev.error == 0, "device link: error {}", dev.error);

    // Idle and drained first. A pending response means the instrument's CPU may
    // be in its interrupt-disabled transfer loop, where a command byte hangs.
    let w = c.device_write(dev.lid, b"*IDN?\n", true, 5000).await?;
    anyhow::ensure!(w.error == 0, "write: error {}", w.error);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let r = c.device_read(dev.lid, 512, 5000, None).await?;
    println!(
        "before: {:?}",
        String::from_utf8_lossy(&r.data).trim().to_string()
    );

    let intf = c.create_link("gpib0").await?;
    anyhow::ensure!(intf.error == 0, "interface link: error {}", intf.error);

    // Addressed to listen, then the two configuration bytes under ATN.
    let cmd = [UNL, mla(pad), CFE, CFG15];
    println!("sending under ATN: {cmd:02x?}  (UNL, MLA{pad}, CFE, CFG15)");
    let d = c.device_docmd(intf.lid, 0x020000, true, 1, &cmd).await?;
    anyhow::ensure!(d.error == 0, "send command: error {}", d.error);

    // Self-check. A one-character response is the sharpest probe available:
    // unconfigured, this instrument loses it 100% of the time. The expected
    // answer is pinned, never probed -- a probed reference would define the
    // empty string as correct and report success on a broken instrument.
    let mut lost = 0usize;
    let trials = 20;
    for _ in 0..trials {
        let w = c.device_write(dev.lid, b"*OPC?\n", true, 5000).await?;
        anyhow::ensure!(w.error == 0, "write during check: error {}", w.error);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let r = c.device_read(dev.lid, 512, 5000, None).await?;
        if String::from_utf8_lossy(&r.data).trim() != "1" {
            lost += 1;
        }
    }
    println!(
        "check:  *OPC? returned \"1\" in {}/{trials} delayed reads",
        trials - lost
    );
    if lost == 0 {
        println!("\nConfigured. This lasts until the instrument is power cycled, and");
        println!("nothing reports it, so re-run this after every power-on.");
        Ok(())
    } else {
        anyhow::bail!(
            "{lost}/{trials} reads still lost their response -- the configuration did not take"
        )
    }
}
