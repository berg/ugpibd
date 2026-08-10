// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Interactive SCPI CLI. Connects to ugpibd over any of its front-ends —
// HiSLIP (IVI-6.1, the default), VXI-11, or the Prologix line protocol —
// and runs a request/response REPL: queries (lines the quote-aware hint
// calls queries) print the instrument's reply, plain commands are written,
// and `++` meta-commands map to control operations. `++read` performs an
// explicit addressed read on the transports whose protocol has one (VXI-11,
// Prologix) — the escape hatch for instruments that only produce output
// once addressed to talk. Uses rustyline for line editing + history.

use anyhow::{Context, Result};
use clap::Parser;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Config, Editor};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use tokio::runtime::{Builder, Runtime};
use ugpibd::hislip::client::HislipClient;
use ugpibd::hislip::server::query_hint;
use ugpibd::vxi11::client::Vxi11Client;

/// Vendor id the client advertises in the HiSLIP Initialize handshake.
const CLIENT_VENDOR_ID: u16 = 0xBEEF;

#[derive(Parser)]
#[command(
    name = "ugpibd-scpi",
    version,
    about = "Interactive SCPI client for ugpibd (HiSLIP)",
    help_template = ugpibd::HELP_TEMPLATE
)]
struct Args {
    /// ugpibd host
    #[arg(long, default_value = "localhost")]
    host: String,

    /// Which front-end to speak.
    #[arg(long, value_enum, default_value_t = TransportKind::Hislip)]
    transport: TransportKind,

    /// Port to connect to. Defaults per transport: 4880 (hislip),
    /// 9010 (vxi11), 1234 (prologix).
    #[arg(long)]
    port: Option<u16>,

    /// GPIB primary address to talk to. Omit to use the daemon's default
    /// PAD.
    #[arg(long)]
    addr: Option<u8>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum TransportKind {
    Hislip,
    Vxi11,
    Prologix,
}

impl TransportKind {
    fn default_port(self) -> u16 {
        match self {
            Self::Hislip => ugpibd::hislip::STANDARD_PORT,
            Self::Vxi11 => ugpibd::vxi11::server::DEFAULT_PORT,
            Self::Prologix => 1234,
        }
    }
}

/// What the REPL needs from a connection, whatever the wire protocol.
///
/// `read` is the explicit addressed read — the operation HiSLIP's protocol
/// cannot express (the server pushes; there is no read request), which is
/// exactly why the other transports exist. The REPL's `++read` maps to it.
#[async_trait::async_trait(?Send)]
trait Transport {
    async fn write(&mut self, cmd: &[u8]) -> Result<()>;
    async fn query(&mut self, cmd: &[u8]) -> Result<Vec<u8>>;
    async fn read(&mut self) -> Result<Vec<u8>>;
    async fn clear(&mut self) -> Result<()>;
    async fn trigger(&mut self) -> Result<()>;
    async fn remote(&mut self, on: bool) -> Result<()>;
    async fn status(&mut self) -> Result<u8>;
}

#[async_trait::async_trait(?Send)]
impl Transport for HislipClient {
    async fn write(&mut self, cmd: &[u8]) -> Result<()> {
        HislipClient::write(self, cmd).await
    }
    async fn query(&mut self, cmd: &[u8]) -> Result<Vec<u8>> {
        HislipClient::query(self, cmd).await
    }
    async fn read(&mut self) -> Result<Vec<u8>> {
        anyhow::bail!(
            "HiSLIP has no client-driven read; use --transport vxi11 (or prologix)              for instruments that only talk when addressed"
        )
    }
    async fn clear(&mut self) -> Result<()> {
        HislipClient::clear(self).await
    }
    async fn trigger(&mut self) -> Result<()> {
        HislipClient::trigger(self).await
    }
    async fn remote(&mut self, on: bool) -> Result<()> {
        HislipClient::remote(self, on).await
    }
    async fn status(&mut self) -> Result<u8> {
        HislipClient::status(self).await
    }
}

/// VXI-11: one link on one core connection.
struct Vxi11Transport {
    client: Vxi11Client,
    lid: i32,
}

impl Vxi11Transport {
    /// VXI-11 errors arrive as in-band codes; the REPL wants words.
    fn check(error: u32, doing: &str) -> Result<()> {
        if error == 0 {
            return Ok(());
        }
        anyhow::bail!("{doing}: VXI-11 error {error}");
    }
}

#[async_trait::async_trait(?Send)]
impl Transport for Vxi11Transport {
    async fn write(&mut self, cmd: &[u8]) -> Result<()> {
        // The terminator matters on real instruments: a bare command with
        // EOI is accepted everywhere, so send exactly what was typed.
        let resp = self.client.device_write(self.lid, cmd, true, 0).await?;
        Vxi11Transport::check(resp.error, "write")
    }
    async fn query(&mut self, cmd: &[u8]) -> Result<Vec<u8>> {
        Transport::write(self, cmd).await?;
        Transport::read(self).await
    }
    async fn read(&mut self) -> Result<Vec<u8>> {
        let resp = self.client.device_read(self.lid, 65536, 0, None).await?;
        Vxi11Transport::check(resp.error, "read")?;
        Ok(resp.data)
    }
    async fn clear(&mut self) -> Result<()> {
        Vxi11Transport::check(self.client.device_clear(self.lid).await?, "clear")
    }
    async fn trigger(&mut self) -> Result<()> {
        Vxi11Transport::check(self.client.device_trigger(self.lid).await?, "trigger")
    }
    async fn remote(&mut self, on: bool) -> Result<()> {
        let error = if on {
            self.client.device_remote(self.lid).await?
        } else {
            self.client.device_local(self.lid).await?
        };
        Vxi11Transport::check(error, "remote/local")
    }
    async fn status(&mut self) -> Result<u8> {
        let resp = self.client.device_readstb(self.lid, 0).await?;
        Vxi11Transport::check(resp.error, "serial poll")?;
        Ok(resp.stb)
    }
}

/// Prologix: the line protocol, client-driven reads by design.
struct PrologixTransport {
    stream: std::net::TcpStream,
}

impl PrologixTransport {
    fn connect(host: &str, port: u16, addr: Option<u8>) -> Result<Self> {
        use std::io::Write as _;
        let stream = std::net::TcpStream::connect((host, port))
            .with_context(|| format!("connect to prologix front-end {host}:{port}"))?;
        stream.set_read_timeout(Some(std::time::Duration::from_millis(3500)))?;
        let mut t = Self { stream };
        t.send_line("++auto 0")?;
        if let Some(pad) = addr {
            let mut line = String::from("++addr ");
            line.push_str(&pad.to_string());
            t.send_line(&line)?;
        }
        let _ = t.stream.flush();
        Ok(t)
    }

    fn send_line(&mut self, line: &str) -> Result<()> {
        use std::io::Write as _;
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(
            b"
",
        )?;
        Ok(())
    }

    /// Read one response: bytes until the read timeout closes the exchange.
    fn read_response(&mut self) -> Result<Vec<u8>> {
        use std::io::Read as _;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.ends_with(
                        b"
",
                    ) {
                        break;
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break
                }
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::ensure!(!buf.is_empty(), "no response before the read timeout");
        Ok(buf)
    }
}

#[async_trait::async_trait(?Send)]
impl Transport for PrologixTransport {
    async fn write(&mut self, cmd: &[u8]) -> Result<()> {
        self.send_line(std::str::from_utf8(cmd).context("prologix commands are text")?)
    }
    async fn query(&mut self, cmd: &[u8]) -> Result<Vec<u8>> {
        Transport::write(self, cmd).await?;
        Transport::read(self).await
    }
    async fn read(&mut self) -> Result<Vec<u8>> {
        self.send_line("++read eoi")?;
        self.read_response()
    }
    async fn clear(&mut self) -> Result<()> {
        self.send_line("++clr")
    }
    async fn trigger(&mut self) -> Result<()> {
        self.send_line("++trg")
    }
    async fn remote(&mut self, on: bool) -> Result<()> {
        if on {
            anyhow::bail!("the prologix front-end asserts REN at ++mode 1; use ++loc for local")
        }
        self.send_line("++loc")
    }
    async fn status(&mut self) -> Result<u8> {
        self.send_line("++spoll")?;
        let resp = self.read_response()?;
        let text = String::from_utf8_lossy(&resp);
        text.trim()
            .parse::<u8>()
            .with_context(|| format!("unexpected ++spoll reply {:?}", text.trim()))
    }
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
    let port = args.port.unwrap_or_else(|| args.transport.default_port());

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create tokio runtime")?;

    let target = format!("{}:{}", args.host, port);
    let (mut transport, describe): (Box<dyn Transport>, String) = match args.transport {
        TransportKind::Hislip => {
            let subaddress = match args.addr {
                Some(n) => format!("hislip{n}"),
                None => ugpibd::hislip::DEFAULT_SUBADDRESS.to_string(),
            };
            let client = rt
                .block_on(HislipClient::connect(
                    &args.host,
                    port,
                    &subaddress,
                    CLIENT_VENDOR_ID,
                ))
                .with_context(|| format!("connect to {target}"))?;
            (Box::new(client), format!("hislip as {subaddress}"))
        }
        TransportKind::Vxi11 => {
            let device = match args.addr {
                Some(n) => format!("gpib0,{n}"),
                None => "inst0".to_string(),
            };
            let mut client = rt
                .block_on(Vxi11Client::connect(&args.host, port))
                .with_context(|| format!("connect to {target}"))?;
            let link = rt.block_on(client.create_link(&device))?;
            anyhow::ensure!(
                link.error == 0,
                "create_link for {device:?} refused: VXI-11 error {}",
                link.error
            );
            let lid = link.lid;
            (
                Box::new(Vxi11Transport { client, lid }),
                format!("vxi11 as {device}"),
            )
        }
        TransportKind::Prologix => {
            let t = PrologixTransport::connect(&args.host, port, args.addr)?;
            let described = match args.addr {
                Some(n) => format!("prologix at pad {n}"),
                None => "prologix (daemon default pad)".to_string(),
            };
            (Box::new(t), described)
        }
    };

    let interactive = std::io::stdin().is_terminal();
    if interactive {
        eprintln!("[connected to {target}, {describe}]  (Ctrl-D to quit)");
    } else {
        eprintln!("[connected to {target}, {describe}]");
    }

    if interactive {
        run_interactive(&rt, transport.as_mut())
    } else {
        run_batch(&rt, transport.as_mut())
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
fn handle_line(rt: &Runtime, client: &mut dyn Transport, line: &str) -> Step {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Step::Continue;
    }

    let result: Result<()> = if let Some(rest) = trimmed.strip_prefix("++") {
        handle_meta(rt, client, rest)
    } else if query_hint(trimmed.as_bytes()) {
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
fn handle_meta(rt: &Runtime, client: &mut dyn Transport, rest: &str) -> Result<()> {
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
        "read" => {
            let resp = rt.block_on(client.read())?;
            print_response(&resp);
            Ok(())
        }
        "status" | "stb" | "spoll" => {
            let stb = rt.block_on(client.status())?;
            println!("{stb}");
            Ok(())
        }
        "help" => {
            eprintln!("meta-commands: ++read ++clr ++trg ++ren <0|1> ++status ++help");
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

fn run_interactive(rt: &Runtime, client: &mut dyn Transport) -> Result<()> {
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

fn run_batch(rt: &Runtime, client: &mut dyn Transport) -> Result<()> {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("stdin read")?;
        if let Step::Disconnected = handle_line(rt, client, &line) {
            break;
        }
    }
    Ok(())
}
