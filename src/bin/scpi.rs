// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors
//
// Tiny interactive SCPI/Prologix CLI. Connects to gpibd over TCP and forwards
// stdin lines verbatim; server replies stream to stdout from a reader thread.

use anyhow::{Context, Result};
use clap::Parser;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::thread;

#[derive(Parser)]
#[command(name = "scpi", about = "Simple SCPI/Prologix TCP client for gpibd")]
struct Args {
    /// gpibd host
    #[arg(long, default_value = "localhost")]
    host: String,

    /// gpibd port
    #[arg(long, default_value_t = 1234)]
    port: u16,

    /// Initial GPIB address to select (skips if omitted)
    #[arg(long)]
    addr: Option<u8>,

    /// Enable auto-read (`++auto 1`) on startup
    #[arg(long)]
    auto: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let target = format!("{}:{}", args.host, args.port);
    let stream = TcpStream::connect(&target)
        .with_context(|| format!("failed to connect to {target}"))?;
    eprintln!("[connected to {target}]");

    // Reader thread: prints every line the server sends.
    let read_stream = stream.try_clone().context("clone stream")?;
    thread::spawn(move || {
        let mut reader = BufReader::new(read_stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    eprintln!("[server closed connection]");
                    std::process::exit(0);
                }
                Ok(_) => {
                    print!("{line}");
                    let _ = std::io::stdout().flush();
                }
                Err(e) => {
                    eprintln!("[read error: {e}]");
                    std::process::exit(1);
                }
            }
        }
    });

    let mut writer = stream;
    let send = |w: &mut TcpStream, s: &str| -> Result<()> {
        w.write_all(s.as_bytes())?;
        w.write_all(b"\n")?;
        w.flush()?;
        Ok(())
    };

    // Optional startup setup.
    send(&mut writer, "++mode 1")?;
    if let Some(a) = args.addr {
        send(&mut writer, &format!("++addr {a}"))?;
    }
    if args.auto {
        send(&mut writer, "++auto 1")?;
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("stdin read")?;
        if line.is_empty() {
            continue;
        }
        send(&mut writer, &line)?;
    }
    // Give the reader thread a moment to print any in-flight response before
    // we drop the socket.
    thread::sleep(std::time::Duration::from_millis(500));
    Ok(())
}
