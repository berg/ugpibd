// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use ugpibd::hislip;

#[derive(Parser, Debug)]
#[command(
    name = "ugpibd",
    version,
    about = "USB-GPIB daemon with HiSLIP and Prologix TCP front-ends",
    help_template = ugpibd::HELP_TEMPLATE
)]
struct Args {
    /// TCP port for the Prologix-compatible server
    #[arg(long, default_value_t = 1234)]
    port: u16,

    /// TCP port for the HiSLIP server (set to 0 to disable)
    #[arg(long, default_value_t = hislip::STANDARD_PORT)]
    hislip_port: u16,

    /// TCP port for the VXI-11 core channel (set to 0 to disable). VXI-11
    /// has no fixed port; clients that cannot be told one (NI/Keysight VISA
    /// hardwire portmapper discovery) need the portmapper, which is separate
    /// and off by default. pyvisa-py carries the port in the resource
    /// string: TCPIP::<host>,<port>::gpib0,<pad>::INSTR
    #[arg(long, default_value_t = ugpibd::vxi11::server::DEFAULT_PORT)]
    vxi11_port: u16,

    /// Bind address
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// GPIB timeout in milliseconds
    #[arg(long, default_value_t = 3000)]
    timeout_ms: u32,

    /// USB-GPIB adapter backend: "auto" to detect by USB VID/PID, "list" to
    /// print the known backends and exit, or a specific backend id.
    #[arg(long, default_value = "auto")]
    backend: String,

    /// Enable the Prologix-compatible front-end (disabled by default).
    #[arg(long)]
    enable_prologix: bool,

    /// Start in unaddressed-listen ("listen only") mode, for capturing from a
    /// talk-only instrument. This only sets the initial state: the mode is
    /// switched at runtime with `++lon`, so a socket- or systemd-activated
    /// daemon is never stuck in the wrong one.
    ///
    /// Mutually exclusive with ordinary traffic while it is on — writes are
    /// refused, and reads take whatever is on the bus rather than addressing
    /// an instrument.
    #[arg(long)]
    listen_only: bool,

    /// Come up as a GPIB *device* at this primary address instead of as a
    /// controller, for instruments that drive their own plot transfer.
    ///
    /// Releases system control: no IFC, no REN, and we answer only when some
    /// other controller addresses us. An SR620 with its plotter address set to
    /// 5 emits nothing at all until a device exists at 5 — see docs/CAPTURE.md.
    ///
    /// Like --listen-only this only sets the initial state; `++dev` switches it
    /// at runtime.
    #[arg(long)]
    listen_address: Option<u8>,

    /// TCP port for the raw capture stream (0 disables, the default).
    ///
    /// On connect it streams bytes off the bus and nothing else — no framing,
    /// no headers — so `nc localhost 1235 > plot.bin` is the whole client.
    /// Requires listen-only mode; see `docs/CAPTURE.md`.
    #[arg(long, default_value_t = 0)]
    capture_port: u16,

    /// List attached supported USB-GPIB adapters (with their port ids) and exit.
    #[arg(long)]
    list: bool,

    /// Select the adapter at this USB port id (from --list). Needed when more
    /// than one supported adapter is attached.
    #[arg(long)]
    usb_port: Option<String>,

    /// Default GPIB primary address used when a request does not specify one:
    /// the fallback PAD for HiSLIP clients using a bare "hislip0"/"gpib0"
    /// subaddress, and the initial Prologix "++addr".
    #[arg(long, default_value_t = 0)]
    default_address: u8,

    /// Increase log verbosity: -v enables debug (dumps HiSLIP cmd/response
    /// bytes with non-printables escaped), -vv enables trace. Ignored if
    /// RUST_LOG is set in the environment.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match args.verbose {
            0 => "ugpibd=info",
            1 => "ugpibd=debug",
            _ => "ugpibd=trace",
        };
        EnvFilter::new(level)
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();

    if args.backend == "list" {
        for kind in ugpibd::backend::BackendKind::ALL {
            println!("{:<16}  {}", kind.id(), kind.description());
        }
        return Ok(());
    }

    if args.list {
        let found = ugpibd::backend::select::enumerate()?;
        if found.is_empty() {
            println!("no supported USB-GPIB adapters found");
        } else {
            println!(
                "{:<3} {:<16} {:<10} {:<14} {:<18} product",
                "#", "backend", "vid:pid", "port", "serial"
            );
            for (i, a) in found.iter().enumerate() {
                println!(
                    "{:<3} {:<16} {:<10} {:<14} {:<18} {}",
                    i,
                    a.kind.id(),
                    format!("{:04x}:{:04x}", a.vid, a.pid),
                    a.port_id,
                    a.serial.as_deref().unwrap_or("(none)"),
                    a.product.as_deref().unwrap_or(""),
                );
            }
        }
        return Ok(());
    }

    // Logged unconditionally at info: journalctl output for a service is usually
    // the only evidence available when triaging a report, and knowing which build
    // produced it is the first question.
    info!("ugpibd {} starting", ugpibd::VERSION);
    if !args.enable_prologix && args.hislip_port == 0 {
        anyhow::bail!(
            "no front-end enabled: pass --enable-prologix and/or a nonzero --hislip-port"
        );
    }
    let backend = match args.backend.as_str() {
        "auto" => None,
        id => Some(id),
    };
    let selector = match args.usb_port {
        Some(ref port) => ugpibd::backend::select::UsbSelector::Port(port.clone()),
        None => ugpibd::backend::select::UsbSelector::Auto,
    };
    let (ctrl, adapter_port) =
        ugpibd::backend::open_selected(&selector, backend, args.timeout_ms).await?;

    if let Some(addr) = args.listen_address {
        ctrl.lock()
            .await
            .set_device_mode(Some(addr))
            .await
            .context("--listen-address requested but the adapter refused it")?;
        info!("started as a GPIB device at address {addr}; ++dev off returns to controller mode");
    }

    if args.listen_only {
        // Fail loudly rather than starting in the wrong mode: a daemon that
        // silently came up as an ordinary controller would present a listener
        // that is never ready, and the talk-only instrument it was started for
        // would refuse to transmit with no indication why.
        ctrl.lock()
            .await
            .set_listen_only(true)
            .await
            .context("--listen-only requested but the adapter refused it")?;
        info!("started in listen-only mode; ++lon 0 returns to controller mode");
    }

    let prologix_listener = if args.enable_prologix {
        let l = TcpListener::bind(format!("{}:{}", args.bind, args.port)).await?;
        info!("prologix listening on {}:{}", args.bind, args.port);
        Some(l)
    } else {
        None
    };

    let hislip_listener = if args.hislip_port != 0 {
        let l = TcpListener::bind(format!("{}:{}", args.bind, args.hislip_port)).await?;
        info!("hislip listening on {}:{}", args.bind, args.hislip_port);
        Some(l)
    } else {
        None
    };

    let capture_listener = if args.capture_port != 0 {
        let l = TcpListener::bind(format!("{}:{}", args.bind, args.capture_port)).await?;
        info!("capture listening on {}:{}", args.bind, args.capture_port);
        Some(l)
    } else {
        None
    };

    let vxi11_listener = if args.vxi11_port != 0 {
        let l = TcpListener::bind(format!("{}:{}", args.bind, args.vxi11_port)).await?;
        info!("vxi11 listening on {}:{}", args.bind, args.vxi11_port);
        Some(l)
    } else {
        None
    };

    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    let capture_ctrl = ctrl.clone();
    let capture_fut = async move {
        match capture_listener {
            Some(listener) => ugpibd::capture::run(listener, capture_ctrl).await,
            None => std::future::pending::<Result<()>>().await,
        }
    };

    let prologix_ctrl = ctrl.clone();
    let hislip_ctrl = ctrl.clone();
    let default_pad = args.default_address;
    let locks = Arc::new(ugpibd::frontend::lock::LockRegistry::new());

    let prologix_fut = async move {
        match prologix_listener {
            Some(listener) => ugpibd::server::run(listener, prologix_ctrl, default_pad).await,
            None => std::future::pending::<Result<()>>().await,
        }
    };

    let hislip_fut = async move {
        match hislip_listener {
            Some(listener) => {
                let device_for = move |subaddr: &str| {
                    let pad = hislip::server::parse_subaddress_pad(subaddr).unwrap_or(default_pad);
                    let dev: Arc<dyn hislip::server::Device> = Arc::new(
                        hislip::instrument::GpibInstrument::new(hislip_ctrl.clone(), pad),
                    );
                    Some(dev)
                };
                // One lock registry for the daemon, not per front-end: a
                // viLock excludes I/O on the instrument no matter which
                // protocol carries it. Today only HiSLIP enforces locks; the
                // VXI-11 front-end will share this same registry.
                let config = hislip::server::Config {
                    locks: locks.clone(),
                    ..Default::default()
                };
                hislip::server::run(listener, config, device_for)
                    .await
                    .map_err(anyhow::Error::from)
            }
            None => std::future::pending::<Result<()>>().await,
        }
    };

    let vxi11_ctrl = ctrl.clone();
    let vxi11_fut = async move {
        match vxi11_listener {
            Some(listener) => {
                let config = ugpibd::vxi11::server::Config {
                    default_io_timeout_ms: args.timeout_ms,
                    default_pad,
                    ..Default::default()
                };
                let instrument_for = move |pad: u8| {
                    Arc::new(ugpibd::frontend::instrument::Instrument::new(
                        vxi11_ctrl.clone(),
                        pad,
                    ))
                };
                ugpibd::vxi11::server::run(listener, config, instrument_for)
                    .await
                    .map_err(anyhow::Error::from)
            }
            None => std::future::pending::<Result<()>>().await,
        }
    };

    // Watch the USB port the adapter is on. Waiting for a transfer to fail
    // would never fire while the daemon is idle, leaving it running against
    // hardware that is no longer there and answering clients with errors it
    // cannot recover from.
    let removal = ugpibd::backend::select::wait_for_removal(&adapter_port);

    let mut adapter_gone = false;
    let result = tokio::select! {
        result = prologix_fut => result,
        result = hislip_fut => result,
        result = capture_fut => result,
        result = vxi11_fut => result,
        _ = removal => {
            info!("adapter at USB port {adapter_port} was unplugged, shutting down");
            adapter_gone = true;
            Ok(())
        }
        _ = ctrl_c => {
            info!("SIGINT received, shutting down");
            Ok(())
        }
        _ = sigterm.recv() => {
            info!("SIGTERM received, shutting down");
            Ok(())
        }
    };

    // Leave the adapter clean for the next run even if a front-end failed;
    // some adapters keep GPIB state across host restarts. Skip it when the
    // adapter has been unplugged: every transfer would fail, and warning about
    // failing to tidy up hardware that is gone is just noise.
    if adapter_gone {
        info!("skipping adapter shutdown, it is no longer attached");
    } else if let Err(e) = ctrl.lock().await.shutdown().await {
        warn!("adapter shutdown failed: {e:#}");
    }

    result
}
