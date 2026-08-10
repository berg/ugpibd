// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// ugpibd-portmap: a fixed-table ONC-RPC portmapper (RFC 1833 §3) answering
// GETPORT/DUMP/NULL for the daemon's VXI-11 core channel.
//
// A separate binary on purpose. Port 111 is a system-wide service and the
// one thing here that wants privileges; quarantining it keeps the
// instrument daemon's hardening profile untouched and makes the whole
// feature an optional package. The only coupling to ugpibd is a port
// number, which is static configuration — GETPORT returns a port and
// nothing else, the host being implied by whom the client asked.
//
// pyvisa-py needs none of this (the resource string carries the port); it
// exists for VISA stacks that hardwire portmap discovery — NI, Keysight —
// and for `rpcinfo -p`.

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
    about = "Portmapper (RFC 1833) advertising ugpibd's VXI-11 core channel",
    help_template = ugpibd::HELP_TEMPLATE
)]
struct Args {
    /// Bind address. Must match where clients reach the daemon; portmap
    /// answers with a port and the asking client connects to the same host.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// TCP+UDP port to serve portmap on. The protocol's fixed port is 111,
    /// which needs privileges to bind directly — under systemd, enable
    /// ugpibd-portmap.socket instead and the listeners arrive pre-bound.
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
}

/// Adopt listeners passed by systemd socket activation, if any.
///
/// The contract (sd_listen_fds): LISTEN_PID names this process, LISTEN_FDS
/// sockets start at fd 3, ordered as declared in the unit —
/// ugpibd-portmap.socket declares ListenStream then ListenDatagram, so fd 3
/// is the TCP listener and fd 4 the UDP socket. Anything else is a
/// misconfigured unit and refuses loudly rather than serving half a
/// protocol.
#[cfg(unix)]
fn systemd_sockets() -> Result<Option<(std::net::TcpListener, std::net::UdpSocket)>> {
    use std::os::fd::FromRawFd;

    let Ok(pid) = std::env::var("LISTEN_PID") else {
        return Ok(None);
    };
    if pid != std::process::id().to_string() {
        return Ok(None);
    }
    let count: u32 = std::env::var("LISTEN_FDS")
        .context("LISTEN_PID set but LISTEN_FDS missing")?
        .parse()
        .context("LISTEN_FDS is not a number")?;
    anyhow::ensure!(
        count == 2,
        "expected exactly 2 activated sockets (ListenStream then ListenDatagram), got {count}"
    );
    // SAFETY: systemd hands these fds to us for ownership, per the
    // sd_listen_fds contract checked above; nothing else in this process
    // knows about fds 3 and 4.
    let (tcp, udp) = unsafe {
        (
            std::net::TcpListener::from_raw_fd(3),
            std::net::UdpSocket::from_raw_fd(4),
        )
    };
    // Verify the unit ordering really gave stream-then-dgram: a UDP socket
    // has no listen backlog, so accepting on it would fail at runtime in
    // confusing ways. local_addr succeeds on both; the type check is the
    // accept probe below at bind time in tokio conversion (a dgram fd in
    // TcpListener errors on first accept). Cheap sanity: both must have
    // addresses.
    tcp.local_addr()
        .context("activated fd 3 has no local address (is it the ListenStream?)")?;
    udp.local_addr()
        .context("activated fd 4 has no local address (is it the ListenDatagram?)")?;
    Ok(Some((tcp, udp)))
}

#[cfg(not(unix))]
fn systemd_sockets() -> Result<Option<(std::net::TcpListener, std::net::UdpSocket)>> {
    Ok(None)
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

    let (tcp, udp) = match systemd_sockets()? {
        Some((tcp, udp)) => {
            info!("adopting systemd-activated sockets");
            (tcp, udp)
        }
        None => {
            let addr = format!("{}:{}", args.bind, args.port);
            let tcp = std::net::TcpListener::bind(&addr).with_context(|| {
                format!(
                    "bind {addr}/tcp — port {} needs privileges to bind directly; \
                     under systemd, enable ugpibd-portmap.socket instead, and if \
                     rpcbind owns the port, stop it or serve elsewhere with --port",
                    args.port
                )
            })?;
            let udp =
                std::net::UdpSocket::bind(&addr).with_context(|| format!("bind {addr}/udp"))?;
            (tcp, udp)
        }
    };
    tcp.set_nonblocking(true)?;
    udp.set_nonblocking(true)?;
    let tcp = tokio::net::TcpListener::from_std(tcp)?;
    let udp = tokio::net::UdpSocket::from_std(udp)?;
    let port = tcp.local_addr()?.port();

    let mappings = vec![
        Mapping {
            prog: DEVICE_CORE_PROG,
            vers: DEVICE_CORE_VERS,
            prot: portmap::IPPROTO_TCP,
            port: u32::from(args.core_port),
        },
        // The portmapper lists itself, as rpcbind does; `rpcinfo -p` output
        // starts with these rows and some clients sanity-check them.
        Mapping {
            prog: PMAP_PROG,
            vers: PMAP_VERS,
            prot: portmap::IPPROTO_TCP,
            port: u32::from(port),
        },
        Mapping {
            prog: PMAP_PROG,
            vers: PMAP_VERS,
            prot: portmap::IPPROTO_UDP,
            port: u32::from(port),
        },
    ];

    portmap::run(tcp, udp, mappings).await?;
    Ok(())
}
