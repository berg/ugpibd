// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 gpibd contributors

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::gpib::{GpibController, Transport};
use crate::prologix::{LineResult, PrologixState};

/// Run the TCP server. Accepts one connection at a time.
pub async fn run<T: Transport>(
    listener: TcpListener,
    mut ctrl: GpibController<T>,
) -> Result<()> {
    info!("Prologix TCP server listening on {}", listener.local_addr()?);

    loop {
        let (mut stream, addr) = listener.accept().await?;
        info!(%addr, "client connected");
        match handle_connection(&mut stream, &mut ctrl).await {
            Ok(()) => info!(%addr, "client disconnected"),
            Err(e) => warn!(%addr, "client error: {e:#}"),
        }
    }
}

async fn handle_connection<T: Transport>(
    stream: &mut TcpStream,
    ctrl: &mut GpibController<T>,
) -> Result<()> {
    let (reader, mut writer) = stream.split();
    let mut lines = BufReader::new(reader).lines();
    let mut state = PrologixState::default();

    while let Some(line) = lines.next_line().await? {
        debug!("< {line:?}");
        match state.handle_line(&line) {
            LineResult::Ok => {}
            LineResult::Response(r) => {
                debug!("> {r:?}");
                writer.write_all(r.as_bytes()).await?;
                writer.write_all(b"\n").await?;
            }
            LineResult::Error(e) => {
                warn!("prologix error: {e}");
                let msg = format!("error: {e}\n");
                writer.write_all(msg.as_bytes()).await?;
            }
            LineResult::Forward {
                pad,
                data,
                send_eoi,
                auto_read,
            } => {
                ctrl.write(pad, &data, send_eoi).await?;
                if auto_read {
                    let (resp, _eom) = ctrl.read(pad, 65536).await?;
                    let resp = state.apply_eot(resp);
                    debug!("> {} bytes", resp.len());
                    writer.write_all(&resp).await?;
                    writer.write_all(b"\n").await?;
                }
            }
            LineResult::Read { .. } => {
                let (resp, _eom) = ctrl.read(state.addr, 65536).await?;
                let resp = state.apply_eot(resp);
                debug!("> {} bytes", resp.len());
                writer.write_all(&resp).await?;
                writer.write_all(b"\n").await?;
            }
            LineResult::DeviceClear { pad } => {
                ctrl.device_clear(pad).await?;
            }
            LineResult::Ifc => {
                ctrl.ifc().await?;
            }
            LineResult::Reset => {
                ctrl.init(0).await?;
                state = PrologixState::default();
            }
        }
    }
    Ok(())
}
