// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// ugpibd-portmap: portmap presence for the daemon's VXI-11 core channel,
// picking its own mode.
//
// At startup it probes localhost:111. A portmapper answering (rpcbind —
// Debian and Raspberry Pi OS install it by default) means cooperate:
// register program 0x0607AF with it via PMAPPROC_SET, the way classic
// ONC-RPC services announce themselves, then stay resident keeping the
// registration alive across rpcbind restarts and withdrawing it on
// shutdown. Nobody answering means serve: bind port 111 and answer
// GETPORT/DUMP/NULL directly (the systemd unit grants
// CAP_NET_BIND_SERVICE for exactly this).
//
// One unit, one enable, no mode decision for the operator — the fighting
// alternative (Conflicts= against rpcbind) was tried and lost, turning
// into a restart war on systems that ship rpcbind.
//
// A separate binary on purpose: port 111 is a system-wide service, and
// quarantining it keeps the instrument daemon's hardening profile
// untouched. The only coupling to ugpibd is --core-port.

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use ugpibd::vxi11::portmap::{self, Mapping, PMAP_PORT, PMAP_PROG, PMAP_VERS};
use ugpibd::vxi11::{DEVICE_CORE_PROG, DEVICE_CORE_VERS};

#[derive(Parser, Debug)]
#[command(
    name = "ugpibd-portmap",
    version,
    about = "Portmap presence (RFC 1833) for ugpibd's VXI-11 core channel",
    help_template = ugpibd::HELP_TEMPLATE
)]
struct Args {
    /// Bind address for serve mode. Must match where clients reach the
    /// daemon; portmap answers with a port and the asking client connects
    /// to the same host.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// The portmap port: probed (and registered with) in register mode,
    /// bound in serve mode.
    #[arg(long, default_value_t = PMAP_PORT)]
    port: u16,

    /// The port ugpibd's VXI-11 core channel listens on — what GETPORT
    /// answers for program 0x0607AF. Must match the daemon's --vxi11-port.
    #[arg(long, default_value_t = ugpibd::vxi11::server::DEFAULT_PORT)]
    core_port: u16,

    /// Increase log verbosity: -v enables debug, -vv trace. Ignored if
    /// RUST_LOG is set in the environment.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    mode: Option<Mode>,
}

#[derive(clap::Subcommand, Debug)]
enum Mode {
    /// Serve the portmap protocol directly, without probing first.
    Serve,
    /// Register with an already-running portmapper and exit (one-shot; the
    /// registration is not maintained).
    Register,
    /// Withdraw the registration and exit.
    Unregister,
}

fn core_mapping(core_port: u16) -> Mapping {
    Mapping {
        prog: DEVICE_CORE_PROG,
        vers: DEVICE_CORE_VERS,
        prot: portmap::IPPROTO_TCP,
        port: u32::from(core_port),
    }
}

/// Register mode: announce, maintain, withdraw on shutdown.
async fn run_registered(port: u16, mapping: Mapping) -> Result<()> {
    let accepted = portmap::set_registration("127.0.0.1", port, mapping, true)
        .await
        .context("registering with the system portmapper")?;
    anyhow::ensure!(
        accepted,
        "the portmapper refused to register program {DEVICE_CORE_PROG:#x} \
         (a conflicting registration? `rpcinfo -p` to inspect)"
    );
    info!(
        "registered VXI-11 core (program {DEVICE_CORE_PROG:#x} -> tcp {}) with the system portmapper",
        mapping.port
    );
    tokio::select! {
        _ = portmap::maintain_registration("127.0.0.1", port, mapping) => unreachable!(),
        _ = shutdown_signal() => {}
    }
    info!("withdrawing portmap registration");
    match portmap::set_registration("127.0.0.1", port, mapping, false).await {
        Ok(true) => {}
        Ok(false) => info!("registration was already gone"),
        Err(e) => info!("could not withdraw registration: {e:#}"),
    }
    Ok(())
}

/// Serve mode: own the port outright.
async fn run_serving(bind: &str, port: u16, mapping: Mapping) -> Result<()> {
    let addr = format!("{bind}:{port}");
    let tcp = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| {
            format!(
                "bind {addr}/tcp — binding port {port} directly needs CAP_NET_BIND_SERVICE \
             (the systemd unit grants it); if a portmapper appeared here since startup, \
             restart to pick register mode"
            )
        })?;
    let udp = tokio::net::UdpSocket::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}/udp"))?;
    let self_port = tcp.local_addr()?.port();
    let mappings = vec![
        mapping,
        // The portmapper lists itself, as rpcbind does; `rpcinfo -p` output
        // starts with these rows and some clients sanity-check them.
        Mapping {
            prog: PMAP_PROG,
            vers: PMAP_VERS,
            prot: portmap::IPPROTO_TCP,
            port: u32::from(self_port),
        },
        Mapping {
            prog: PMAP_PROG,
            vers: PMAP_VERS,
            prot: portmap::IPPROTO_UDP,
            port: u32::from(self_port),
        },
    ];
    tokio::select! {
        r = portmap::run(tcp, udp, mappings) => r.map_err(Into::into),
        _ = shutdown_signal() => Ok(()),
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match args.verbose {
            0 => "ugpibd=info,ugpibd_portmap=info",
            1 => "ugpibd=debug,ugpibd_portmap=debug",
            _ => "ugpibd=trace,ugpibd_portmap=trace",
        };
        EnvFilter::new(level)
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();

    info!("ugpibd-portmap {} starting", ugpibd::VERSION);
    let mapping = core_mapping(args.core_port);

    match args.mode {
        Some(Mode::Serve) => run_serving(&args.bind, args.port, mapping).await,
        Some(Mode::Register) => {
            let accepted = portmap::set_registration("127.0.0.1", args.port, mapping, true).await?;
            anyhow::ensure!(accepted, "the portmapper refused the registration");
            info!("registered (one-shot; not maintained)");
            Ok(())
        }
        Some(Mode::Unregister) => {
            let accepted =
                portmap::set_registration("127.0.0.1", args.port, mapping, false).await?;
            anyhow::ensure!(accepted, "no registration to withdraw");
            info!("unregistered");
            Ok(())
        }
        None => {
            // The automagic: cooperate if anyone is home, own the port if not.
            if portmap::probe("127.0.0.1", args.port).await {
                info!("a portmapper answers on port {}; cooperating", args.port);
                run_registered(args.port, mapping).await
            } else {
                info!("no portmapper on port {}; serving it", args.port);
                run_serving(&args.bind, args.port, mapping).await
            }
        }
    }
}
