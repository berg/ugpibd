// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Exercise the VXI-11.2 interface link against a running daemon: bus status
// selectors from the live lines, a harmless Send Command (UNL), bus-wide
// DCL, and the docmd-on-device-link refusal. A bench diagnostic for the
// interface-device surface.
//
//     cargo run --example vxi11_interface_probe -- [host] [port] [device-pad]

use ugpibd::vxi11::client::Vxi11Client;
use ugpibd::vxi11::ErrorCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.next().map_or(Ok(9010), |p| p.parse())?;
    let pad: u8 = args.next().map_or(Ok(23), |p| p.parse())?;

    let mut client = Vxi11Client::connect(&host, port).await?;
    let intf = client.create_link("gpib0").await?;
    anyhow::ensure!(intf.error == 0, "interface link refused: {}", intf.error);
    println!("interface link {} (abort port {})", intf.lid, intf.abort_port);

    const NAMES: [&str; 8] = [
        "REMOTE",
        "SRQ",
        "NDAC",
        "SYSTEM CONTROLLER",
        "CONTROLLER-IN-CHARGE",
        "TALKER",
        "LISTENER",
        "BUS ADDRESS",
    ];
    for (i, name) in NAMES.iter().enumerate() {
        let selector = (i as u16 + 1).to_be_bytes();
        let resp = client
            .device_docmd(intf.lid, 0x020001, true, 2, &selector)
            .await?;
        anyhow::ensure!(resp.error == 0, "bus status {name}: error {}", resp.error);
        let value = u16::from_be_bytes([resp.data_out[0], resp.data_out[1]]);
        println!("bus status {name:<22} = {value}");
    }

    // Send Command: UNL alone is harmless on any bus.
    let resp = client.device_docmd(intf.lid, 0x020000, true, 1, &[0x3F]).await?;
    anyhow::ensure!(resp.error == 0, "send command: error {}", resp.error);
    println!("send command (UNL) ok, echoed {:02x?}", resp.data_out);

    // Bus-wide device clear through the interface link.
    let error = client.device_clear(intf.lid).await?;
    anyhow::ensure!(error == 0, "interface clear: error {error}");
    println!("interface clear (DCL) ok");

    // A device link must refuse docmd (RULE B.5.2) but still answer queries.
    let dev = client.create_link(&format!("gpib0,{pad}")).await?;
    anyhow::ensure!(dev.error == 0, "device link refused: {}", dev.error);
    let resp = client.device_docmd(dev.lid, 0x020000, true, 1, &[0x3F]).await?;
    anyhow::ensure!(
        resp.error == ErrorCode::OperationNotSupported.as_u32(),
        "docmd on device link should be 8, got {}",
        resp.error
    );
    println!("docmd on device link correctly refused (8)");

    client.device_write(dev.lid, b"*IDN?\n", true, 0).await?;
    let idn = client.device_read(dev.lid, 256, 5000, None).await?;
    anyhow::ensure!(idn.error == 0, "post-DCL query: error {}", idn.error);
    println!(
        "device at pad {pad} alive after DCL: {}",
        String::from_utf8_lossy(&idn.data).trim()
    );
    println!("ALL PASS");
    Ok(())
}
