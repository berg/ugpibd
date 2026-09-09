// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Compare serial-poll command orderings against a live instrument, through a
// VXI-11 interface link (Send Command + unaddressed read), for instruments
// that answer one ordering and not another.
//
// The daemon's own ordering puts SPE ahead of the addresses in a single
// command block; the linux-gpib driver addresses first, sends SPE, and only
// then sends the talk address in a separate transfer. IEEE-488.1 allows both,
// so an instrument that answers only one of them is the thing worth knowing.
//
// Read the control line before concluding anything about the instrument: a
// poll is a one-byte read, so a backend that drops a byte fails every ordering
// here and looks exactly like an instrument that ignores SPE. Run the same
// instrument on a second adapter before blaming it.
//
//     cargo run --example serial_poll_probe -- [host] [port] [device-pad]

use ugpibd::vxi11::client::Vxi11Client;

const UNL: u8 = 0x3f;
const UNT: u8 = 0x5f;
const SPE: u8 = 0x18;
const SPD: u8 = 0x19;

fn mta(pad: u8) -> u8 {
    0x40 | pad
}
fn mla(pad: u8) -> u8 {
    0x20 | pad
}

/// Bus status selector 2 is the SRQ line.
async fn srq(client: &mut Vxi11Client, intf_lid: i32) -> anyhow::Result<u16> {
    let resp = client
        .device_docmd(intf_lid, 0x020001, true, 2, &2u16.to_be_bytes())
        .await?;
    anyhow::ensure!(resp.error == 0, "bus status SRQ: error {}", resp.error);
    Ok(u16::from_be_bytes([resp.data_out[0], resp.data_out[1]]))
}

async fn write(client: &mut Vxi11Client, lid: i32, cmd: &str) -> anyhow::Result<()> {
    let resp = client
        .device_write(lid, format!("{cmd}\n").as_bytes(), true, 2000)
        .await?;
    anyhow::ensure!(resp.error == 0, "write {cmd:?}: error {}", resp.error);
    Ok(())
}

async fn query(client: &mut Vxi11Client, lid: i32, cmd: &str) -> anyhow::Result<String> {
    write(client, lid, cmd).await?;
    let resp = client.device_read(lid, 256, 2000, None).await?;
    anyhow::ensure!(resp.error == 0, "query {cmd:?}: error {}", resp.error);
    Ok(String::from_utf8_lossy(&resp.data).trim().to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.next().map_or(Ok(9010), |p| p.parse())?;
    let pad: u8 = args.next().map_or(Ok(4), |p| p.parse())?;

    let mut client = Vxi11Client::connect(&host, port).await?;
    let intf = client.create_link("gpib0").await?;
    anyhow::ensure!(intf.error == 0, "interface link refused: {}", intf.error);

    // Each ordering, as the sequence of Send Command blocks that precede the
    // one-byte read. A block boundary is where the daemon drops to standby and
    // takes control again, which is the only difference the bus can see.
    let orderings: [(&str, Vec<Vec<u8>>); 4] = [
        (
            "ugpibd: SPE ahead of the addresses, one block",
            vec![vec![UNL, SPE, mta(pad), mla(0)]],
        ),
        (
            "linux-gpib: UNL+MLA+SPE, then MTA in its own block",
            vec![vec![UNL, mla(0), SPE], vec![mta(pad)]],
        ),
        (
            "addresses first, SPE last, one block",
            vec![vec![UNL, mla(0), mta(pad), SPE]],
        ),
        (
            "SPE in its own block, then the addresses",
            vec![vec![UNL, SPE], vec![mta(pad), mla(0)]],
        ),
    ];

    for (name, blocks) in orderings {
        for block in &blocks {
            let resp = client
                .device_docmd(intf.lid, 0x020000, true, 1, block)
                .await?;
            anyhow::ensure!(
                resp.error == 0,
                "{name}: send command: error {}",
                resp.error
            );
        }
        let read = client.device_read(intf.lid, 1, 2000, None).await?;
        if read.error == 0 && !read.data.is_empty() {
            println!("{name:<52} -> status {:#04x}", read.data[0]);
        } else {
            println!(
                "{name:<52} -> nothing (error {}, {} bytes)",
                read.error,
                read.data.len()
            );
        }
        // Put the bus back before the next ordering, whatever happened.
        client
            .device_docmd(intf.lid, 0x020000, true, 1, &[SPD, UNT])
            .await?;
    }

    // Control: the same interface-link read, but of an ordinary query
    // response. If this comes back and the polls do not, the instrument is
    // ignoring serial poll rather than the probe reading the bus wrongly.
    let dev = client.create_link(&format!("gpib0,{pad}")).await?;
    anyhow::ensure!(dev.error == 0, "device link refused: {}", dev.error);
    let w = client.device_write(dev.lid, b"*IDN?\n", true, 2000).await?;
    anyhow::ensure!(w.error == 0, "control write: error {}", w.error);
    client
        .device_docmd(intf.lid, 0x020000, true, 1, &[UNL, mla(0), mta(pad)])
        .await?;
    let read = client.device_read(intf.lid, 64, 2000, None).await?;
    println!(
        "control: unaddressed read of the *IDN? response -> error {}, {:?}",
        read.error,
        String::from_utf8_lossy(&read.data)
    );
    client
        .device_docmd(intf.lid, 0x020000, true, 1, &[UNT, UNL])
        .await?;

    // Whether the instrument drives SRQ at all. An instrument that ignores the
    // poll but still asserts the line is usable for service requests — the
    // daemon sees the line, and *STB? stands in for the byte the poll would
    // have carried. One that does neither cannot report service at all, which
    // is a much bigger limitation and worth separating.
    let sre = query(&mut client, dev.lid, "*SRE?").await?;
    write(&mut client, dev.lid, "*SRE 16").await?; // request service on MAV
    write(&mut client, dev.lid, "*IDN?").await?; // response left queued: MAV
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    println!(
        "SRQ line with a response queued and *SRE 16: {}",
        srq(&mut client, intf.lid).await?
    );

    // Drain the response, restore the mask the instrument came with.
    let drained = client.device_read(dev.lid, 256, 2000, None).await?;
    println!(
        "  (queued response: {:?})",
        String::from_utf8_lossy(&drained.data).trim()
    );
    write(&mut client, dev.lid, &format!("*SRE {}", sre.trim())).await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    println!(
        "SRQ line after draining it: {}",
        srq(&mut client, intf.lid).await?
    );

    client.destroy_link(dev.lid).await?;
    client.destroy_link(intf.lid).await?;
    Ok(())
}
