// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;
use tracing_subscriber::EnvFilter;

use ugpibd::hislip;

#[derive(Parser, Debug)]
#[command(
    name = "ugpibd",
    about = "Agilent/Keysight 82357B USB-GPIB daemon (Prologix + HiSLIP compatible)"
)]
struct Args {
    /// TCP port for the Prologix-compatible server
    #[arg(long, default_value_t = 1234)]
    port: u16,

    /// TCP port for the HiSLIP server (set to 0 to disable)
    #[arg(long, default_value_t = hislip::STANDARD_PORT)]
    hislip_port: u16,

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
        .with_writer(std::io::stderr)
        .init();

    if args.backend == "list" {
        for kind in ugpibd::backend::BackendKind::ALL {
            println!("{:<16}  {}", kind.id(), kind.description());
        }
        return Ok(());
    }

    info!("ugpibd starting");
    if !args.enable_prologix && args.hislip_port == 0 {
        anyhow::bail!(
            "no front-end enabled: pass --enable-prologix and/or a nonzero --hislip-port"
        );
    }
    let selection = match args.backend.as_str() {
        "auto" => None,
        id => Some(id),
    };
    let ctrl = ugpibd::backend::open_selected(selection, args.timeout_ms).await?;

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

    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    let prologix_ctrl = ctrl.clone();
    let hislip_ctrl = ctrl.clone();
    let default_pad = args.default_address;

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
                hislip::server::run(listener, hislip::server::Config::default(), device_for)
                    .await
                    .map_err(anyhow::Error::from)
            }
            None => std::future::pending::<Result<()>>().await,
        }
    };

    tokio::select! {
        result = prologix_fut => result?,
        result = hislip_fut => result?,
        _ = ctrl_c => info!("SIGINT received, shutting down"),
        _ = sigterm.recv() => info!("SIGTERM received, shutting down"),
    }

    Ok(())
}
