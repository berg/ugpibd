// SPDX-License-Identifier: GPL-3.0-or-later
//
// End-to-end test of the `ugpibd-scpi` CLI binary: runs the real compiled binary in
// batch (non-TTY) mode against an in-process HiSLIP echo server and asserts
// it speaks the protocol correctly (query → reply, write, ++ meta-commands).

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use ugpibd::hislip::server::{run, Config, Device, Execution};

#[derive(Default)]
struct EchoDevice {
    cleared: AtomicBool,
}

#[async_trait::async_trait]
impl Device for EchoDevice {
    async fn execute(&self, cmd: &[u8], expect_response: bool) -> Result<Execution> {
        if expect_response {
            Ok(Some(cmd.to_vec()).into())
        } else {
            Ok(None.into())
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
    async fn get_status(&self) -> Result<u8> {
        Ok(0)
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

    let mut child = Command::new(env!("CARGO_BIN_EXE_ugpibd-scpi"))
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ugpibd-scpi");

    {
        let mut stdin = child.stdin.take().unwrap();
        // a write (no reply), a meta-command, and a query (echoed back).
        stdin.write_all(b"*RST\n++clr\nHELLO?\n").unwrap();
        // dropping stdin closes it -> batch loop ends -> process exits.
    }

    let out = child.wait_with_output().expect("wait ugpibd-scpi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("HELLO?"),
        "expected query echo in stdout, got: {stdout:?} (stderr: {:?})",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The same batch exchange over the VXI-11 transport, against a VXI-11
/// server backed by an echoing mock: the CLI's --transport flag selects the
/// front-end, and ++read performs an explicit addressed read.
#[test]
fn batch_over_vxi11_transport() {
    use ugpibd::backend::GpibBackend;
    use ugpibd::frontend::instrument::Instrument;

    /// Echo instrument: every read answers the last write.
    struct EchoBackend {
        last: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl GpibBackend for EchoBackend {
        async fn init(&mut self, _my_pad: u8) -> Result<()> {
            Ok(())
        }
        async fn write(&mut self, _pad: u8, data: &[u8], _send_eoi: bool) -> Result<()> {
            self.last = data.to_vec();
            Ok(())
        }
        async fn read(&mut self, _pad: u8, max_len: usize) -> Result<(Vec<u8>, bool)> {
            let mut d = std::mem::take(&mut self.last);
            d.truncate(max_len);
            Ok((d, true))
        }
        async fn send_data_unaddressed(&mut self, data: &[u8], _send_eoi: bool) -> Result<()> {
            self.last.extend_from_slice(data);
            Ok(())
        }
        async fn read_unaddressed(&mut self, max_len: usize) -> Result<(Vec<u8>, bool)> {
            let mut d = std::mem::take(&mut self.last);
            d.truncate(max_len);
            Ok((d, true))
        }
        async fn device_clear(&mut self, _pad: u8) -> Result<()> {
            Ok(())
        }
        async fn trigger(&mut self, _pad: u8) -> Result<()> {
            Ok(())
        }
        async fn ifc(&mut self) -> Result<()> {
            Ok(())
        }
        async fn ren(&mut self, _enable: bool) -> Result<()> {
            Ok(())
        }
        async fn go_to_remote(&mut self, _pad: u8) -> Result<()> {
            Ok(())
        }
        async fn go_to_local(&mut self, _pad: u8) -> Result<()> {
            Ok(())
        }
        async fn local_lockout(&mut self) -> Result<()> {
            Ok(())
        }
        async fn serial_poll(&mut self, _pad: u8) -> Result<u8> {
            Ok(0x42)
        }
        async fn set_controller_pad(&mut self, _pad: u8) -> Result<()> {
            Ok(())
        }
        fn controller_pad(&self) -> u8 {
            0
        }
        fn set_eos(&mut self, _eos_char: u8, _enabled: bool) {}
        fn eos(&self) -> (u8, bool) {
            (b'\n', false)
        }
        fn set_timeout(&mut self, _timeout_ms: u32) {}
        fn name(&self) -> &'static str {
            "echo"
        }
    }

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            let ctrl: Arc<tokio::sync::Mutex<dyn GpibBackend>> =
                Arc::new(tokio::sync::Mutex::new(EchoBackend { last: Vec::new() }));
            let instrument_for = move |pad: u8| Arc::new(Instrument::new(ctrl.clone(), pad));
            let _ = ugpibd::vxi11::server::run(
                listener,
                ugpibd::vxi11::server::Config::default(),
                instrument_for,
            )
            .await;
        });
    });
    let port = rx.recv().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ugpibd-scpi"))
        .args([
            "--host",
            "127.0.0.1",
            "--transport",
            "vxi11",
            "--port",
            &port.to_string(),
            "--addr",
            "7",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ugpibd-scpi");

    {
        let mut stdin = child.stdin.take().unwrap();
        // A query (echo), a plain write followed by an explicit ++read (the
        // 8594E-shaped flow), and a serial poll.
        stdin
            .write_all(b"HELLO?\nPLAIN WRITE\n++read\n++status\n")
            .unwrap();
    }

    let out = child.wait_with_output().expect("wait ugpibd-scpi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("HELLO?"),
        "query echo missing: {stdout:?} ({stderr:?})"
    );
    assert!(
        stdout.contains("PLAIN WRITE"),
        "++read after write missing: {stdout:?}"
    );
    assert!(stdout.contains("66"), "++status (0x42) missing: {stdout:?}");
}
