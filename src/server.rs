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
            LineResult::SerialPoll { pad } => match ctrl.lock().await.serial_poll(pad).await {
                Ok(stb) => {
                    writer.write_all(format!("{stb}\n").as_bytes()).await?;
                }
                Err(e) => warn!("gpib serial_poll failed: {e:#}"),
            },
            LineResult::Trigger { pad } => {
                if let Err(e) = ctrl.lock().await.trigger(pad).await {
                    warn!("gpib trigger failed: {e:#}");
                }
            }
            LineResult::Srq => match ctrl.lock().await.srq_asserted().await {
                Ok(asserted) => {
                    let v = u8::from(asserted);
                    writer.write_all(format!("{v}\n").as_bytes()).await?;
                }
                // Never fall back to "0": that is indistinguishable from a
                // healthy bus with nothing requesting service.
                Err(e) => warn!("gpib srq check failed: {e:#}"),
            },
            LineResult::ListenOnly(None) => {
                let v = u8::from(ctrl.lock().await.listen_only());
                writer.write_all(format!("{v}\n").as_bytes()).await?;
            }
            LineResult::ListenOnly(Some(enable)) => {
                if let Err(e) = ctrl.lock().await.set_listen_only(enable).await {
                    warn!("gpib set_listen_only({enable}) failed: {e:#}");
                    writer
                        .write_all(format!("error: {e}\n").as_bytes())
                        .await?;
                }
            }
            LineResult::DeviceMode(None) => {
                let msg = match ctrl.lock().await.device_address() {
                    Some(a) => a.to_string(),
                    None => "off".to_string(),
                };
                writer.write_all(format!("{msg}\n").as_bytes()).await?;
            }
            LineResult::DeviceMode(Some(target)) => {
                if let Err(e) = ctrl.lock().await.set_device_mode(target).await {
                    warn!("gpib set_device_mode({target:?}) failed: {e:#}");
                    writer
                        .write_all(format!("error: {e}\n").as_bytes())
                        .await?;
                }
            }
            LineResult::BusLines => match ctrl.lock().await.bus_lines().await {
                Ok(lines) => {
                    writer.write_all(format!("{lines}\n").as_bytes()).await?;
                }
                // Same rule as ++srq: no invented all-clear reading, because it
                // would be indistinguishable from a real one.
                Err(e) => warn!("gpib bus line read failed: {e:#}"),
            },
            LineResult::DeviceClear { pad } => {
                if let Err(e) = ctrl.lock().await.device_clear(pad).await {
                    warn!("gpib device_clear failed: {e:#}");
                }
            }
            LineResult::GoToLocal { pad } => {
                if let Err(e) = ctrl.lock().await.go_to_local(pad).await {
                    warn!("gpib go_to_local failed: {e:#}");
                }
            }
            LineResult::LocalLockout => {
                if let Err(e) = ctrl.lock().await.local_lockout().await {
                    warn!("gpib local_lockout failed: {e:#}");
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
