// SPDX-License-Identifier: GPL-3.0-or-later
//
// Tests for the HiSLIP client used by the `scpi` CLI. Each test drives a
// real in-process HiSLIP server (the same one `ugpibd` runs) over a loopback
// TCP socket, so the client exercises the actual wire protocol end to end.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use ugpibd::hislip::client::HislipClient;
use ugpibd::hislip::server::{run, Config, Device};

/// A device that records control operations and echoes data so the client's
/// round-trips can be asserted.
#[derive(Default)]
struct ProbeDevice {
    triggered: AtomicBool,
    cleared: AtomicBool,
    remote: AtomicBool,
    status: AtomicU8,
}

#[async_trait::async_trait]
impl Device for ProbeDevice {
    async fn execute(&self, cmd: &[u8], expect_response: bool) -> Result<Option<Vec<u8>>> {
        if !expect_response {
            return Ok(None);
        }
        if cmd.eq_ignore_ascii_case(b"*idn?") {
            Ok(Some(b"ECHO,TEST,SN,1.0\n".to_vec()))
        } else {
            Ok(Some(cmd.to_vec()))
        }
    }
    async fn trigger(&self) -> Result<()> {
        self.triggered.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn clear(&self) -> Result<()> {
        self.cleared.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn set_remote(&self, remote: bool) -> Result<()> {
        self.remote.store(remote, Ordering::SeqCst);
        Ok(())
    }
    async fn get_status(&self) -> u8 {
        self.status.load(Ordering::SeqCst)
    }
}

async fn start_server(dev: Arc<ProbeDevice>) -> Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let dev = dev.clone();
        let _ = run(listener, Config::default(), move |_subaddr| {
            let d: Arc<dyn Device> = dev.clone();
            Some(d)
        })
        .await;
    });
    Ok(addr)
}

async fn connect(addr: std::net::SocketAddr) -> Result<HislipClient> {
    HislipClient::connect(&addr.ip().to_string(), addr.port(), "hislip0", 0x1234).await
}

#[tokio::test]
async fn query_round_trips() {
    let dev = Arc::new(ProbeDevice::default());
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    let resp = client.query(b"*IDN?").await.unwrap();
    assert_eq!(resp, b"ECHO,TEST,SN,1.0\n");
}

#[tokio::test]
async fn write_sends_no_read() {
    let dev = Arc::new(ProbeDevice::default());
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    // A write must not block waiting for a reply, and a subsequent query on
    // the same channel must still line up its own response.
    client.write(b"*RST").await.unwrap();
    let resp = client.query(b"echo me?").await.unwrap();
    assert_eq!(resp, b"echo me?");
}

#[tokio::test]
async fn successive_queries_use_incrementing_message_ids() {
    let dev = Arc::new(ProbeDevice::default());
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    // Each query advances the MessageID by 2; the server echoes it and the
    // client verifies the echo, so a mismatch would surface as an error.
    for _ in 0..3 {
        assert_eq!(client.query(b"x?").await.unwrap(), b"x?");
    }
}

#[tokio::test]
async fn trigger_reaches_device() {
    let dev = Arc::new(ProbeDevice::default());
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    client.trigger().await.unwrap();
    // Trigger has no reply; confirm via a follow-up round-trip that the
    // device observed it.
    let _ = client.query(b"sync?").await.unwrap();
    assert!(dev.triggered.load(Ordering::SeqCst));
}

#[tokio::test]
async fn clear_acks_and_reaches_device() {
    let dev = Arc::new(ProbeDevice::default());
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    client.clear().await.unwrap();
    assert!(dev.cleared.load(Ordering::SeqCst));
}

#[tokio::test]
async fn remote_toggles_ren() {
    let dev = Arc::new(ProbeDevice::default());
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    client.remote(true).await.unwrap();
    assert!(dev.remote.load(Ordering::SeqCst));
    client.remote(false).await.unwrap();
    assert!(!dev.remote.load(Ordering::SeqCst));
}

#[tokio::test]
async fn status_reads_status_byte() {
    let dev = Arc::new(ProbeDevice::default());
    dev.status.store(0x42, Ordering::SeqCst);
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    assert_eq!(client.status().await.unwrap(), 0x42);
}
