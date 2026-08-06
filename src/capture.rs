// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// The capture front-end: a raw byte stream off the bus, for talk-only sources.
//
// This exists because of a measured property of the hardware rather than a
// design preference. The adapter is a *ready* listener only while a read is
// actually armed; between reads it re-asserts an RFD holdoff and a talk-only
// talker stops transmitting to it (see `docs/CAPTURE.md` §4.7). A client
// polling `++read` therefore leaves a gap on every round trip, and the talker
// gives up inside it — capturing an HP 53310A that way lost the tail of every
// print. Keeping a read continuously in flight is the whole point of this
// module, and it is why capture cannot just be a Prologix command.
//
// While a capture is running, every other client is delayed by up to one read
// timeout on each operation — measured at 2-3 s against a GPIB-USB-HS+. That is
// the bus genuinely being busy rather than a bug, but it is also why the lock
// is released between reads instead of held for the whole session.
//
// The stream is deliberately featureless: bytes, no framing, no headers, no
// boundaries. Where one plot or print ends is a question only a client with a
// format parser can answer, and this daemon does not have one — see
// `docs/CAPTURE.md` §5.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::backend::{GpibBackend, SharedBackend};

/// Bytes requested per read. Large on purpose: a single armed read is what
/// keeps us a ready listener, so the bigger the window the fewer the gaps. The
/// NI adapter clamps this to its own transfer limit.
const CHUNK: usize = 65536;

/// Serve one capture client, streaming until it disconnects or the mode ends.
///
/// Two failure modes had to be avoided here, and the obvious fix for the first
/// causes the second:
///
/// * A client that disconnects on a **quiet** bus must still be noticed. An
///   earlier version learned of it only when a write to the socket failed, and
///   on an idle bus it never wrote, so the session leaked and held the
///   single-client slot until the daemon restarted.
///
/// * A bus read must **never be cancelled mid-transfer**. Selecting on the
///   socket against the read looks like the natural fix, but dropping that
///   future abandons an in-flight USB bulk transfer. The adapter still sends
///   its response, nobody consumes it, and the next transaction reads that
///   stale response as its own — the NI adapter then fails with "missing chunk
///   start id" and the 82357 with "unexpected response byte". Both were seen,
///   and both make every subsequent measurement quietly wrong.
///
/// So disconnects are detected by a separate task setting a flag, and the flag
/// is checked *between* reads. A departing client therefore waits up to one
/// read timeout to be noticed, which is a fine price for never corrupting the
/// adapter.
async fn stream_to(ctrl: &SharedBackend, sock: &mut TcpStream) -> Result<()> {
    let mut total: u64 = 0;
    let (mut rx, mut tx) = sock.split();

    // A capture client sends nothing, so any readable event is EOF or an error.
    let watcher = async move {
        let mut scratch = [0u8; 1];
        loop {
            match rx.read(&mut scratch).await {
                Ok(0) | Err(_) => return,
                Ok(_) => continue,
            }
        }
    };
    tokio::pin!(watcher);

    loop {
        // Non-blocking check for departure. Cancelling a socket read is safe —
        // tokio's AsyncRead is cancel-safe — which is precisely what is *not*
        // true of the USB transfer below, and why that one is never selected on.
        let mut departed = false;
        tokio::select! {
            biased;
            _ = &mut watcher => departed = true,
            _ = std::future::ready(()) => {}
        }
        if departed {
            info!("capture: client went away after {total} bytes");
            return Ok(());
        }

        let chunk = {
            let mut backend = ctrl.lock().await;
            if !backend.listen_only() && backend.device_address().is_none() {
                info!("capture: no longer capturing, closing stream after {total} bytes");
                return Ok(());
            }
            // Deliberately not calling set_timeout: it is per-backend rather
            // than per-operation (`docs/CAPTURE.md` §4.3), so a capture-length
            // timeout would silently change it for every other client and never
            // restore it. `--timeout-ms` governs how long each read stays armed.
            //
            // `pad` is ignored while capturing: there is nobody to address.
            match backend.read(0, CHUNK).await {
                Ok((data, end)) => {
                    debug!(bytes = data.len(), end, "capture read");
                    data
                }
                // A quiet bus times out. Not a failure; re-arm.
                Err(e) => {
                    debug!("capture read returned nothing: {e:#}");
                    Vec::new()
                }
            }
        };

        if chunk.is_empty() {
            continue;
        }
        total += chunk.len() as u64;

        // No buffering, deliberately. If the client cannot keep up, the write
        // blocks, we stop re-arming reads, and the talker stalls on the GPIB
        // handshake — backpressure all the way to the instrument instead of
        // unbounded memory growth here. The cost is documented in
        // `docs/CAPTURE.md` §14.5: a stalled talker holds NRFD and NDAC, which
        // blocks the whole bus and not only this transfer.
        tx.write_all(&chunk).await.context("capture client write")?;
    }
}

/// Accept capture clients on `listener` and stream the bus to them.
///
/// One client at a time, matching the rest of the daemon: two clients cannot
/// both consume one byte stream, and splitting it between them would give each
/// a corrupted half.
pub async fn run(listener: TcpListener, ctrl: SharedBackend) -> Result<()> {
    let busy: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    loop {
        let (mut sock, peer) = listener.accept().await?;

        {
            let mut taken = busy.lock().await;
            if *taken {
                // Say so rather than dropping the connection: a client that is
                // silently disconnected cannot tell "already capturing" from
                // "daemon is broken".
                let _ = sock
                    .write_all(b"error: capture already in progress\n")
                    .await;
                continue;
            }
            *taken = true;
        }

        let in_mode = {
            let b = ctrl.lock().await;
            b.listen_only() || b.device_address().is_some()
        };
        if !in_mode {
            let _ = sock
                .write_all(b"error: not capturing; use ++lon 1 or ++dev <addr>\n")
                .await;
            *busy.lock().await = false;
            continue;
        }

        info!("capture client connected addr={peer}");
        let ctrl2 = ctrl.clone();
        let busy2 = busy.clone();
        tokio::spawn(async move {
            if let Err(e) = stream_to(&ctrl2, &mut sock).await {
                warn!("capture client error: {e:#} addr={peer}");
            } else {
                info!("capture client disconnected addr={peer}");
            }
            *busy2.lock().await = false;
        });
    }
}

/// Whether a backend is currently in a state where capture can produce data.
/// Exposed so the front-ends can report it without duplicating the rule.
pub fn can_capture(backend: &dyn GpibBackend) -> bool {
    backend.listen_only() || backend.device_address().is_some()
}
