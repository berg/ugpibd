// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "gpibd",
    about = "Agilent/Keysight 82357B USB-GPIB daemon (Prologix-compatible)"
)]
struct Args {
    /// TCP port for the Prologix-compatible server
    #[arg(long, default_value_t = 1234)]
    port: u16,

    /// Bind address
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// GPIB timeout in milliseconds
    #[arg(long, default_value_t = 3000)]
    timeout_ms: u32,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("gpibd=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    info!("gpibd starting — looking for 82357B");
    let transport = gpibd::usb::initialize_device(args.timeout_ms).await?;
    info!("USB device open");

    let mut ctrl = gpibd::gpib::GpibController::new(transport, args.timeout_ms);

    // Try up to 3 times: abort -> init. If init fails it's usually because
    // the device is holding stale state from a prior session; another abort
    // (flush + finalize) typically unsticks it.
    let mut last_err = None;
    for attempt in 1..=3 {
        let _ = ctrl.abort(true).await; // flush pending
        let _ = ctrl.abort(false).await; // finalize
        match ctrl.init(0).await {
            Ok(()) => {
                info!("GPIB controller initialized (attempt {attempt})");
                last_err = None;
                break;
            }
            Err(e) => {
                tracing::warn!("init attempt {attempt} failed: {e:#}");
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }

    let listener = TcpListener::bind(format!("{}:{}", args.bind, args.port)).await?;
    info!("listening on {}:{}", args.bind, args.port);

    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    tokio::select! {
        result = gpibd::server::run(listener, ctrl) => result?,
        _ = ctrl_c => info!("SIGINT received, shutting down"),
        _ = sigterm.recv() => info!("SIGTERM received, shutting down"),
    }

    Ok(())
}
