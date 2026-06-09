// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors
//
// Interactive SCPI CLI. Connects to gpibd over HiSLIP (IVI-6.1) and runs a
// request/response REPL: queries (lines containing `?`) print the
// instrument's reply, plain commands are written, and a small set of `++`
// meta-commands map to HiSLIP control operations. Uses rustyline for line
// editing + history.

use anyhow::{Context, Result};
use clap::Parser;
use gpibd::hislip::client::HislipClient;
use gpibd::hislip::STANDARD_PORT;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Config, Editor};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use tokio::runtime::{Builder, Runtime};

/// Vendor id the client advertises in the HiSLIP Initialize handshake.
const CLIENT_VENDOR_ID: u16 = 0xBEEF;

#[derive(Parser)]
#[command(name = "scpi", about = "Interactive SCPI client for gpibd (HiSLIP)")]
struct Args {
    /// gpibd host
    #[arg(long, default_value = "localhost")]
    host: String,

    /// gpibd HiSLIP port
    #[arg(long, default_value_t = STANDARD_PORT)]
    port: u16,

    /// GPIB primary address to talk to. Encoded as the HiSLIP sub-address
    /// `hislip<N>` at connect time. Omit to use the daemon's default PAD
    /// (sub-address `hislip0`).
    #[arg(long)]
    addr: Option<u8>,
}

fn history_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".scpi_history"))
}

fn restore_terminal() {
    // Fallback: ask stty to reset to sane defaults. Runs when rustyline's
    // own Drop impl couldn't (panic, abnormal exit from another thread, etc.).
    let _ = std::process::Command::new("stty").arg("sane").status();
}

fn main() -> Result<()> {
    // Always reset terminal on panic so the user doesn't land in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let args = Args::parse();
    let subaddress = match args.addr {
        Some(n) => format!("hislip{n}"),
        None => gpibd::hislip::DEFAULT_SUBADDRESS.to_string(),
    };

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create tokio runtime")?;
    let mut client = rt
        .block_on(HislipClient::connect(
            &args.host,
            args.port,
            &subaddress,
            CLIENT_VENDOR_ID,
        ))
        .with_context(|| format!("connect to {}:{}", args.host, args.port))?;

    let target = format!("{}:{}", args.host, args.port);
    let interactive = std::io::stdin().is_terminal();
    if interactive {
        eprintln!("[connected to {target} as {subaddress}]  (Ctrl-D to quit)");
    } else {
        eprintln!("[connected to {target} as {subaddress}]");
    }

    if interactive {
        run_interactive(&rt, &mut client)
    } else {
        run_batch(&rt, &mut client)
    }
}

/// Outcome of handling one input line.
enum Step {
    /// Line handled; continue the REPL.
    Continue,
    /// The connection is gone; stop the REPL.
    Disconnected,
}

/// Execute one input line against the instrument, printing any output.
fn handle_line(rt: &Runtime, client: &mut HislipClient, line: &str) -> Step {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Step::Continue;
    }

    let result: Result<()> = if let Some(rest) = trimmed.strip_prefix("++") {
        handle_meta(rt, client, rest)
    } else if trimmed.contains('?') {
        rt.block_on(client.query(trimmed.as_bytes()))
            .map(|resp| print_response(&resp))
    } else {
        rt.block_on(client.write(trimmed.as_bytes()))
    };

    if let Err(e) = result {
        eprintln!("[error: {e:#}]");
        // An I/O error means the socket is gone; nothing more will work.
        if e.chain().any(|c| c.is::<std::io::Error>()) {
            return Step::Disconnected;
        }
    }
    Step::Continue
}

/// Handle a `++` meta-command (the part after `++`).
fn handle_meta(rt: &Runtime, client: &mut HislipClient, rest: &str) -> Result<()> {
    let mut parts = rest.split_whitespace();
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let arg = parts.next();
    match cmd.as_str() {
        "clr" | "cls" => rt.block_on(client.clear()),
        "trg" => rt.block_on(client.trigger()),
        "ren" => {
            let on = match arg {
                Some("0") | Some("off") => false,
                Some("1") | Some("on") | None => true,
                Some(other) => anyhow::bail!("++ren expects 0/1/on/off, got {other:?}"),
            };
            rt.block_on(client.remote(on))
        }
        "status" | "stb" | "spoll" => {
            let stb = rt.block_on(client.status())?;
            println!("{stb}");
            Ok(())
        }
        "help" => {
            eprintln!("meta-commands: ++clr ++trg ++ren <0|1> ++status ++help");
            Ok(())
        }
        other => anyhow::bail!("unknown meta-command ++{other} (try ++help)"),
    }
}

/// Print an instrument response to stdout, ensuring a trailing newline.
fn print_response(resp: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(resp);
    if !resp.ends_with(b"\n") {
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
}

fn run_interactive(rt: &Runtime, client: &mut HislipClient) -> Result<()> {
    let mut rl: Editor<(), FileHistory> =
        Editor::with_config(Config::builder().auto_add_history(true).build())?;

    let history = history_path();
    if let Some(ref p) = history {
        let _ = rl.load_history(p);
    }

    loop {
        match rl.readline("scpi> ") {
            Ok(line) => {
                if let Step::Disconnected = handle_line(rt, client, &line) {
                    eprintln!("[connection closed]");
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => {
                eprintln!();
                break;
            }
            Err(e) => {
                eprintln!("[readline error: {e}]");
                break;
            }
        }
    }

    if let Some(ref p) = history {
        let _ = rl.save_history(p);
    }
    Ok(())
}

fn run_batch(rt: &Runtime, client: &mut HislipClient) -> Result<()> {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("stdin read")?;
        if let Step::Disconnected = handle_line(rt, client, &line) {
            break;
        }
    }
    Ok(())
}
