// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors
//
// Interactive SCPI/Prologix CLI. Connects to gpibd over TCP and forwards stdin
// lines verbatim; server replies stream to stdout from a reader thread.
// Uses rustyline for line editing + history.

use anyhow::{Context, Result};
use clap::Parser;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Config, Editor, ExternalPrinter};
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;

#[derive(Parser)]
#[command(name = "scpi", about = "Interactive SCPI/Prologix TCP client for gpibd")]
struct Args {
    /// gpibd host
    #[arg(long, default_value = "localhost")]
    host: String,

    /// gpibd port
    #[arg(long, default_value_t = 1234)]
    port: u16,

    /// Initial GPIB address to select (sends `++addr N` on connect)
    #[arg(long)]
    addr: Option<u8>,

    /// Enable auto-read (`++auto 1`) on startup
    #[arg(long)]
    auto: bool,
}

fn history_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".scpi_history"))
}

fn main() -> Result<()> {
    let args = Args::parse();
    let target = format!("{}:{}", args.host, args.port);
    let stream = TcpStream::connect(&target)
        .with_context(|| format!("failed to connect to {target}"))?;
    let interactive = std::io::stdin().is_terminal();
    if interactive {
        eprintln!("[connected to {target}]  (Ctrl-D to quit)");
    } else {
        eprintln!("[connected to {target}]");
    }

    let mut writer = stream.try_clone().context("clone stream for writer")?;
    let send = |w: &mut TcpStream, s: &str| -> Result<()> {
        w.write_all(s.as_bytes())?;
        w.write_all(b"\n")?;
        w.flush()?;
        Ok(())
    };

    // Startup setup.
    send(&mut writer, "++mode 1")?;
    if let Some(a) = args.addr {
        send(&mut writer, &format!("++addr {a}"))?;
    }
    if args.auto {
        send(&mut writer, "++auto 1")?;
    }

    if interactive {
        run_interactive(stream, writer)
    } else {
        run_batch(stream, writer)
    }
}

fn run_interactive(read_side: TcpStream, mut writer: TcpStream) -> Result<()> {
    let mut rl: Editor<(), FileHistory> =
        Editor::with_config(Config::builder().auto_add_history(true).build())?;

    let history = history_path();
    if let Some(ref p) = history {
        let _ = rl.load_history(p);
    }

    let mut printer = rl.create_external_printer()?;
    thread::spawn(move || {
        let mut reader = BufReader::new(read_side);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = printer.print("[server closed connection]\n".into());
                    std::process::exit(0);
                }
                Ok(_) => {
                    let _ = printer.print(line.clone());
                }
                Err(e) => {
                    let _ = printer.print(format!("[read error: {e}]\n"));
                    std::process::exit(1);
                }
            }
        }
    });

    loop {
        match rl.readline("scpi> ") {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Err(e) = writer
                    .write_all(trimmed.as_bytes())
                    .and_then(|_| writer.write_all(b"\n"))
                    .and_then(|_| writer.flush())
                {
                    eprintln!("[send error: {e}]");
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(50));
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

fn run_batch(read_side: TcpStream, mut writer: TcpStream) -> Result<()> {
    // Simple non-TTY mode: stream stdin → socket, stream socket → stdout.
    thread::spawn(move || {
        let mut reader = BufReader::new(read_side);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    std::process::exit(0);
                }
                Ok(_) => {
                    print!("{line}");
                    let _ = std::io::stdout().flush();
                }
                Err(_) => std::process::exit(1),
            }
        }
    });

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("stdin read")?;
        if line.is_empty() {
            continue;
        }
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    // Drain any pending response.
    thread::sleep(std::time::Duration::from_millis(500));
    Ok(())
}
