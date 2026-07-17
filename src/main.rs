// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
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
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// GPIB timeout in milliseconds
    #[arg(long, default_value_t = 3000)]
    timeout_ms: u32,

    /// Default GPIB primary address for HiSLIP clients that do not encode
    /// one in their subaddress (e.g. the bare "hislip0" subaddress).
    #[arg(long, default_value_t = 14)]
    hislip_default_pad: u8,

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

    info!("ugpibd starting — looking for 82357B");
    let transport = ugpibd::usb::initialize_device(args.timeout_ms).await?;
    info!("USB device open");

    let mut ctrl = ugpibd::gpib::GpibController::new(transport, args.timeout_ms);

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

    let ctrl: Arc<Mutex<dyn ugpibd::backend::GpibBackend>> = Arc::new(Mutex::new(ctrl));

    let prologix_listener = TcpListener::bind(format!("{}:{}", args.bind, args.port)).await?;
    info!("prologix listening on {}:{}", args.bind, args.port);

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
    let default_pad = args.hislip_default_pad;

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
        result = ugpibd::server::run(prologix_listener, prologix_ctrl) => result?,
        result = hislip_fut => result?,
        _ = ctrl_c => info!("SIGINT received, shutting down"),
        _ = sigterm.recv() => info!("SIGTERM received, shutting down"),
    }

    Ok(())
}
