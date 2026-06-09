// SPDX-License-Identifier: GPL-3.0-or-later
//
// End-to-end test of the `scpi` CLI binary: runs the real compiled binary in
// batch (non-TTY) mode against an in-process HiSLIP echo server and asserts
// it speaks the protocol correctly (query → reply, write, ++ meta-commands).

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use gpibd::hislip::server::{run, Config, Device};
use tokio::net::TcpListener;

#[derive(Default)]
struct EchoDevice {
    cleared: AtomicBool,
}

#[async_trait::async_trait]
impl Device for EchoDevice {
    async fn execute(&self, cmd: &[u8], expect_response: bool) -> Result<Option<Vec<u8>>> {
        if expect_response {
            Ok(Some(cmd.to_vec()))
        } else {
            Ok(None)
        }
    }
    async fn trigger(&self) -> Result<()> {
        Ok(())
    }
    async fn clear(&self) -> Result<()> {
        self.cleared.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn set_remote(&self, _remote: bool) -> Result<()> {
        Ok(())
    }
}

/// Start an echo server on a background thread with its own runtime and return
/// the bound port.
fn spawn_echo_server() -> u16 {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            let dev = Arc::new(EchoDevice::default());
            let _ = run(listener, Config::default(), move |_sub| {
                let d: Arc<dyn Device> = dev.clone();
                Some(d)
            })
            .await;
        });
    });
    rx.recv().unwrap()
}

#[test]
fn batch_query_and_meta_commands() {
    let port = spawn_echo_server();

    let mut child = Command::new(env!("CARGO_BIN_EXE_scpi"))
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scpi");

    {
        let mut stdin = child.stdin.take().unwrap();
        // a write (no reply), a meta-command, and a query (echoed back).
        stdin.write_all(b"*RST\n++clr\nHELLO?\n").unwrap();
        // dropping stdin closes it -> batch loop ends -> process exits.
    }

    let out = child.wait_with_output().expect("wait scpi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("HELLO?"),
        "expected query echo in stdout, got: {stdout:?} (stderr: {:?})",
        String::from_utf8_lossy(&out.stderr)
    );
}
