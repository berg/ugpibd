// SPDX-License-Identifier: GPL-3.0-or-later
//
// Tests for the HiSLIP client used by the `ugpibd-scpi` CLI. Each test drives a
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
    /// Set to hand out an SRQ subscription; `raise_srq` then fires one.
    srq: Option<tokio::sync::broadcast::Sender<()>>,
    /// Live state of the wired-OR SRQ line. `None` models an adapter that
    /// cannot read it.
    line_asserted: Option<AtomicBool>,
    /// Serial polls answered so far, so a test can make RQS appear late.
    polls: AtomicU8,
}

impl ProbeDevice {
    fn with_srq() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(8);
        Self {
            srq: Some(tx),
            ..Default::default()
        }
    }

    /// A device on a bus whose SRQ line the adapter can read.
    fn with_srq_line() -> Self {
        Self {
            line_asserted: Some(AtomicBool::new(false)),
            ..Self::with_srq()
        }
    }

    fn raise_srq(&self) {
        let _ = self.srq.as_ref().expect("no srq channel").send(());
    }

    fn set_line(&self, asserted: bool) {
        self.line_asserted
            .as_ref()
            .expect("no srq line")
            .store(asserted, Ordering::SeqCst);
    }

    fn poll_count(&self) -> u8 {
        self.polls.load(Ordering::SeqCst)
    }
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
    async fn get_status(&self) -> Result<u8> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(self.status.load(Ordering::SeqCst))
    }
    async fn subscribe_srq(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        self.srq.as_ref().map(|tx| tx.subscribe())
    }
    async fn srq_asserted(&self) -> Result<bool> {
        match self.line_asserted {
            Some(ref v) => Ok(v.load(Ordering::SeqCst)),
            None => anyhow::bail!("probe cannot read the SRQ line"),
        }
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
async fn service_request_is_pushed_and_does_not_desync_the_channel() {
    let dev = Arc::new(ProbeDevice::with_srq());
    // RQS (0x40) must be set or the forwarder treats the SRQ as another
    // device's and drops it.
    dev.status.store(0x40 | 0x20, Ordering::SeqCst);
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    dev.raise_srq();

    // The service request arrives unsolicited, so it can land in the middle of
    // this round-trip. The status call must still get its own reply, and the
    // pushed status byte must be recorded rather than discarded.
    let mut seen = None;
    for _ in 0..20 {
        let stb = client.status().await.unwrap();
        assert_eq!(stb, 0x60, "status round-trip desynced");
        if let Some(srq) = client.take_service_request() {
            seen = Some(srq);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(seen, Some(0x60), "no service request was pushed");
}

#[tokio::test]
async fn service_request_arriving_without_an_edge_is_still_forwarded() {
    // The device has not raised RQS when the notification lands -- another
    // instrument pulled the wired-OR line first. Ours asserts a moment later,
    // producing no edge of its own and so no second notification. Re-polling
    // while the line stays asserted is the only way to see it.
    let dev = Arc::new(ProbeDevice::with_srq_line());
    dev.set_line(true);
    dev.status.store(0x00, Ordering::SeqCst);
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    dev.raise_srq();
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert!(
        dev.poll_count() > 1,
        "forwarder gave up after one poll while SRQ was still asserted"
    );

    // Now our device raises RQS, with no further notification.
    dev.status.store(0x60, Ordering::SeqCst);

    let mut seen = None;
    for _ in 0..40 {
        assert_eq!(client.status().await.unwrap(), 0x60);
        if let Some(srq) = client.take_service_request() {
            seen = Some(srq);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        seen,
        Some(0x60),
        "request that arrived without an edge was lost"
    );
}

#[tokio::test]
async fn recheck_stops_once_the_srq_line_is_released() {
    // Nothing is requesting service and the line is idle: the forwarder must
    // poll once and stop, not spin.
    let dev = Arc::new(ProbeDevice::with_srq_line());
    dev.set_line(false);
    dev.status.store(0x00, Ordering::SeqCst);
    let addr = start_server(dev.clone()).await.unwrap();
    let _client = connect(addr).await.unwrap();

    dev.raise_srq();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        dev.poll_count(),
        1,
        "forwarder kept polling after the SRQ line went idle"
    );
}

#[tokio::test]
async fn recheck_is_bounded_when_the_line_stays_asserted() {
    // A device with no session bound to it is requesting service that nothing
    // here can clear, so the line never releases. The retry must give up
    // rather than poll the bus forever.
    let dev = Arc::new(ProbeDevice::with_srq_line());
    dev.set_line(true);
    dev.status.store(0x00, Ordering::SeqCst);
    let addr = start_server(dev.clone()).await.unwrap();
    let _client = connect(addr).await.unwrap();

    dev.raise_srq();
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    let settled = dev.poll_count();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        dev.poll_count(),
        settled,
        "forwarder is still polling well past the recheck budget"
    );
}

#[tokio::test]
async fn service_request_without_rqs_is_not_forwarded() {
    let dev = Arc::new(ProbeDevice::with_srq());
    // Another instrument pulled the wired-OR SRQ line: our device's poll comes
    // back with RQS clear, so nothing should be pushed for this session.
    dev.status.store(0x10, Ordering::SeqCst);
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    dev.raise_srq();

    for _ in 0..10 {
        assert_eq!(client.status().await.unwrap(), 0x10);
        assert_eq!(
            client.take_service_request(),
            None,
            "forwarded a service request for a device that was not requesting"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn status_reads_status_byte() {
    let dev = Arc::new(ProbeDevice::default());
    dev.status.store(0x42, Ordering::SeqCst);
    let addr = start_server(dev.clone()).await.unwrap();
    let mut client = connect(addr).await.unwrap();

    assert_eq!(client.status().await.unwrap(), 0x42);
}
