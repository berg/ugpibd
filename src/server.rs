// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::backend::GpibBackend;
use crate::prologix::{LineResult, PrologixState};

/// Run the TCP server. Accepts one connection at a time.
pub async fn run(
    listener: TcpListener,
    ctrl: Arc<Mutex<dyn GpibBackend>>,
    default_pad: u8,
) -> Result<()> {
    info!(
        "Prologix TCP server listening on {}",
        listener.local_addr()?
    );

    loop {
        let (mut stream, addr) = listener.accept().await?;
        info!(%addr, "client connected");
        match handle_connection(&mut stream, &ctrl, default_pad).await {
            Ok(()) => info!(%addr, "client disconnected"),
            Err(e) => warn!(%addr, "client error: {e:#}"),
        }
    }
}

async fn handle_connection(
    stream: &mut TcpStream,
    ctrl: &Arc<Mutex<dyn GpibBackend>>,
    default_pad: u8,
) -> Result<()> {
    let (reader, mut writer) = stream.split();
    let mut lines = BufReader::new(reader).lines();
    let mut state = PrologixState::with_addr(default_pad);

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
                let mut c = ctrl.lock().await;
                if let Err(e) = c.write(pad, &data, send_eoi).await {
                    warn!("gpib write failed: {e:#}");
                    continue;
                }
                if auto_read {
                    match c.read(pad, 65536).await {
                        Ok((resp, _eom)) => {
                            drop(c);
                            let resp = state.apply_eot(resp);
                            debug!("> {} bytes", resp.len());
                            writer.write_all(&resp).await?;
                        }
                        Err(e) => warn!("gpib read failed: {e:#}"),
                    }
                }
            }
            LineResult::Read { .. } => {
                let res = ctrl.lock().await.read(state.addr, 65536).await;
                match res {
                    Ok((resp, _eom)) => {
                        let resp = state.apply_eot(resp);
                        debug!("> {} bytes", resp.len());
                        writer.write_all(&resp).await?;
                    }
                    Err(e) => warn!("gpib read failed: {e:#}"),
                }
            }
            LineResult::DeviceClear { pad } => {
                if let Err(e) = ctrl.lock().await.device_clear(pad).await {
                    warn!("gpib device_clear failed: {e:#}");
                }
            }
            LineResult::Ifc => {
                if let Err(e) = ctrl.lock().await.ifc().await {
                    warn!("gpib ifc failed: {e:#}");
                }
            }
            LineResult::Reset => {
                if let Err(e) = ctrl.lock().await.init(0).await {
                    warn!("gpib reset/init failed: {e:#}");
                }
                state = PrologixState::with_addr(default_pad);
            }
        }
    }
    Ok(())
}
