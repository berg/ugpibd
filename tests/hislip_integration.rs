// SPDX-License-Identifier: GPL-3.0-or-later
//
// End-to-end tests for the HiSLIP server: the client-side message codec is the
// same as the server's, so we can drive real TCP sockets without a separate
// client library.

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::{TcpListener, TcpStream};
use ugpibd::hislip::messages::{
    InitializeParameter, InitializeResponseParameter, Message, MessageType,
};
use ugpibd::hislip::protocol::PROTOCOL_2_0;
use ugpibd::hislip::server::{run, Config, Device, Execution};

/// A stand-in instrument. `reply` is what every query answers with, `delay` how
/// long the bus transaction pretends to take, `service_request` the status byte
/// it reports having raised while a query ran, and `resource` which lock scope
/// it belongs to.
struct TestDevice {
    reply: Vec<u8>,
    delay: Duration,
    resource: String,
    service_request: Option<u8>,
    clears: Arc<AtomicU32>,
    /// Last state `set_remote` was asked for: 1 remote, 0 local, -1 never.
    remote: Arc<AtomicI32>,
}

impl TestDevice {
    fn new() -> Self {
        Self {
            reply: b"ECHO,TEST,SN,1.0\n".to_vec(),
            delay: Duration::ZERO,
            resource: "gpib0".to_string(),
            service_request: None,
            clears: Arc::new(AtomicU32::new(0)),
            remote: Arc::new(AtomicI32::new(-1)),
        }
    }

    fn replying(mut self, reply: Vec<u8>) -> Self {
        self.reply = reply;
        self
    }

    fn taking(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Behave like an instrument that pulled SRQ while the query ran.
    fn requesting_service(mut self, stb: u8) -> Self {
        self.service_request = Some(stb);
        self
    }
}

#[async_trait::async_trait]
impl Device for TestDevice {
    async fn execute(&self, cmd: &[u8], expect_response: bool) -> Result<Execution> {
        tokio::time::sleep(self.delay).await;
        if !expect_response {
            return Ok(Execution::default());
        }
        let data = if cmd.eq_ignore_ascii_case(b"*idn?") {
            self.reply.clone()
        } else {
            cmd.to_vec()
        };
        Ok(Execution {
            data: Some(data),
            service_request: self.service_request,
        })
    }
    async fn trigger(&self) -> Result<()> {
        Ok(())
    }
    async fn clear(&self) -> Result<()> {
        self.clears.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn set_remote(&self, remote: bool) -> Result<()> {
        self.remote.store(i32::from(remote), Ordering::SeqCst);
        Ok(())
    }
    async fn get_status(&self) -> Result<u8> {
        Ok(0)
    }
    fn resource_key(&self) -> String {
        self.resource.clone()
    }
}

/// Start a server whose every sub-address resolves to `device`.
async fn start_server_with<F>(make: F) -> std::net::SocketAddr
where
    F: Fn(&str) -> Option<Arc<dyn Device>> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = run(listener, Config::default(), make).await;
    });
    addr
}

async fn start_server() -> std::net::SocketAddr {
    start_server_with(|_| {
        let dev: Arc<dyn Device> = Arc::new(TestDevice::new());
        Some(dev)
    })
    .await
}

fn init_param(protocol: u16, vendor: u16) -> u32 {
    ((protocol as u32) << 16) | vendor as u32
}

/// Both channels of one HiSLIP session, with just enough helpers to exercise
/// the server.
struct Session {
    sync: BufStream<TcpStream>,
    async_ch: BufStream<TcpStream>,
    message_id: u32,
}

impl Session {
    async fn open(addr: std::net::SocketAddr, subaddress: &str) -> Session {
        let mut sync = BufStream::new(TcpStream::connect(addr).await.unwrap());
        MessageType::Initialize
            .message_params(0, init_param(PROTOCOL_2_0.into(), 0x1234))
            .with_payload(subaddress.as_bytes().to_vec())
            .write_to(&mut sync)
            .await
            .unwrap();
        sync.flush().await.unwrap();

        let resp = read_msg(&mut sync).await;
        assert_eq!(resp.message_type, MessageType::InitializeResponse);
        let session_id = InitializeResponseParameter(resp.message_parameter).session_id();

        let mut async_ch = BufStream::new(TcpStream::connect(addr).await.unwrap());
        MessageType::AsyncInitialize
            .message_params(0, session_id as u32)
            .no_payload()
            .write_to(&mut async_ch)
            .await
            .unwrap();
        async_ch.flush().await.unwrap();
        let resp = read_msg(&mut async_ch).await;
        assert_eq!(resp.message_type, MessageType::AsyncInitializeResponse);

        Session {
            sync,
            async_ch,
            message_id: 0xffff_ff00,
        }
    }

    /// Send a command on the sync channel and return the message id it used.
    async fn send(&mut self, cmd: &[u8]) -> u32 {
        let id = self.message_id;
        self.message_id = self.message_id.wrapping_add(2);
        MessageType::DataEnd
            .message_params(0, id)
            .with_payload(cmd.to_vec())
            .write_to(&mut self.sync)
            .await
            .unwrap();
        self.sync.flush().await.unwrap();
        id
    }

    async fn read_sync(&mut self) -> Message {
        read_msg(&mut self.sync).await
    }

    async fn async_transaction(&mut self, request: Message) -> Message {
        request.write_to(&mut self.async_ch).await.unwrap();
        self.async_ch.flush().await.unwrap();
        read_msg(&mut self.async_ch).await
    }

    /// Request a lock; returns the AsyncLockResponse control code. An empty
    /// `name` asks for an exclusive lock.
    async fn lock(&mut self, timeout_ms: u32, name: &str) -> u8 {
        let resp = self
            .async_transaction(
                MessageType::AsyncLock
                    .message_params(1, timeout_ms)
                    .with_payload(name.as_bytes().to_vec()),
            )
            .await;
        assert_eq!(resp.message_type, MessageType::AsyncLockResponse);
        resp.control_code
    }

    async fn unlock(&mut self) -> u8 {
        let resp = self
            .async_transaction(MessageType::AsyncLock.message_params(0, 0).no_payload())
            .await;
        assert_eq!(resp.message_type, MessageType::AsyncLockResponse);
        resp.control_code
    }

    /// (exclusive flag, number of clients holding locks)
    async fn lock_info(&mut self) -> (u8, u32) {
        let resp = self
            .async_transaction(MessageType::AsyncLockInfo.message_params(0, 0).no_payload())
            .await;
        assert_eq!(resp.message_type, MessageType::AsyncLockInfoResponse);
        (resp.control_code, resp.message_parameter)
    }

    async fn declare_max_message_size(&mut self, size: u64) {
        let mut payload = [0u8; 8];
        payload.copy_from_slice(&size.to_be_bytes());
        let resp = self
            .async_transaction(
                MessageType::AsyncMaximumMessageSize
                    .message_params(0, 0)
                    .with_payload(payload.to_vec()),
            )
            .await;
        assert_eq!(
            resp.message_type,
            MessageType::AsyncMaximumMessageSizeResponse
        );
    }

    async fn device_clear(&mut self) -> Message {
        self.async_transaction(
            MessageType::AsyncDeviceClear
                .message_params(0, 0)
                .no_payload(),
        )
        .await
    }
}

async fn read_msg<S>(s: &mut S) -> Message
where
    S: tokio::io::AsyncRead + Unpin,
{
    tokio::time::timeout(Duration::from_secs(2), Message::read_from(s, 1024 * 1024))
        .await
        .expect("read timeout")
        .expect("io error")
        .expect("protocol error")
}

/// Read one message, or `None` if nothing arrives within `within`.
async fn read_msg_within<S>(s: &mut S, within: Duration) -> Option<Message>
where
    S: tokio::io::AsyncRead + Unpin,
{
    match tokio::time::timeout(within, Message::read_from(s, 1024 * 1024)).await {
        Ok(result) => Some(result.expect("io error").expect("protocol error")),
        Err(_) => None,
    }
}

#[tokio::test]
async fn hislip_round_trip_idn_query() {
    let addr = start_server().await;
    let mut session = Session::open(addr, "hislip0").await;

    let id = session.send(b"*IDN?").await;
    let resp = session.read_sync().await;
    assert_eq!(resp.message_type, MessageType::DataEnd);
    assert_eq!(resp.message_parameter, id);
    assert_eq!(resp.payload, b"ECHO,TEST,SN,1.0\n");
}

#[tokio::test]
async fn hislip_rejects_unknown_subaddress() {
    let addr = start_server_with(|sub| {
        if sub == "valid" {
            let dev: Arc<dyn Device> = Arc::new(TestDevice::new());
            Some(dev)
        } else {
            None
        }
    })
    .await;

    let mut s = BufStream::new(TcpStream::connect(addr).await.unwrap());
    MessageType::Initialize
        .message_params(0, init_param(PROTOCOL_2_0.into(), 0))
        .with_payload(b"nope".to_vec())
        .write_to(&mut s)
        .await
        .unwrap();
    s.flush().await.unwrap();

    let resp = read_msg(&mut s).await;
    assert_eq!(resp.message_type, MessageType::FatalError);
}

// ------------------------------------------------------------------- locking

#[tokio::test]
async fn an_exclusive_lock_shuts_out_a_second_client() {
    let addr = start_server().await;
    let mut a = Session::open(addr, "hislip0").await;
    let mut b = Session::open(addr, "hislip0").await;

    assert_eq!(a.lock(1000, "").await, 1, "exclusive lock when free");
    assert_eq!(
        a.lock_info().await,
        (1, 1),
        "exclusive flag set, one client holding"
    );
    // B waits out its timeout and is refused, rather than being told yes.
    assert_eq!(b.lock(200, "").await, 0, "second exclusive lock");
    // A shared lock cannot slip past an exclusive one either.
    assert_eq!(
        b.lock(200, "shared").await,
        0,
        "shared lock while exclusive"
    );
}

#[tokio::test]
async fn a_lock_is_enforced_against_other_clients_io() {
    let addr = start_server().await;
    let mut a = Session::open(addr, "hislip0").await;
    let mut b = Session::open(addr, "hislip0").await;

    // Both work before anybody locks.
    b.send(b"*IDN?").await;
    assert_eq!(b.read_sync().await.message_type, MessageType::DataEnd);

    assert_eq!(a.lock(1000, "").await, 1);

    b.send(b"*IDN?").await;
    let refused = b.read_sync().await;
    assert_eq!(refused.message_type, MessageType::Error);
    assert_eq!(refused.control_code, 128, "device-defined 'locked' code");

    MessageType::Trigger
        .message_params(0, 1)
        .no_payload()
        .write_to(&mut b.sync)
        .await
        .unwrap();
    b.sync.flush().await.unwrap();
    assert_eq!(b.read_sync().await.message_type, MessageType::Error);

    let clear = b.device_clear().await;
    assert_eq!(clear.message_type, MessageType::Error);

    // The holder is unaffected, and so is everyone once it lets go.
    a.send(b"*IDN?").await;
    assert_eq!(a.read_sync().await.message_type, MessageType::DataEnd);
    assert_eq!(a.unlock().await, 1);
    b.send(b"*IDN?").await;
    assert_eq!(b.read_sync().await.message_type, MessageType::DataEnd);
}

#[tokio::test]
async fn shared_locks_have_to_agree_on_a_name() {
    let addr = start_server().await;
    let mut a = Session::open(addr, "hislip0").await;
    let mut b = Session::open(addr, "hislip0").await;

    assert_eq!(a.lock(1000, "k1").await, 2, "shared lock when free");
    assert_eq!(b.lock(200, "k2").await, 0, "different name conflicts");
    assert_eq!(b.lock(1000, "k1").await, 2, "same name shares");
    assert_eq!(a.lock_info().await, (0, 2), "two clients, not exclusive");
    // Sharers can both do I/O.
    for session in [&mut a, &mut b] {
        session.send(b"*IDN?").await;
        assert_eq!(session.read_sync().await.message_type, MessageType::DataEnd);
    }
}

#[tokio::test]
async fn releasing_a_lock_that_is_not_held_is_refused() {
    let addr = start_server().await;
    let mut a = Session::open(addr, "hislip0").await;

    assert_eq!(a.unlock().await, 3, "release without ever locking");
    assert_eq!(a.lock(1000, "").await, 1);
    assert_eq!(a.unlock().await, 1);
    assert_eq!(a.unlock().await, 3, "second release");
}

#[tokio::test]
async fn locks_are_scoped_to_the_instrument() {
    let addr = start_server_with(|sub| {
        let mut dev = TestDevice::new();
        dev.resource = sub.to_string();
        let dev: Arc<dyn Device> = Arc::new(dev);
        Some(dev)
    })
    .await;
    let mut a = Session::open(addr, "hislip23").await;
    let mut b = Session::open(addr, "hislip3").await;

    assert_eq!(a.lock(1000, "").await, 1);
    // A lock on the DMM has no business locking out the counter.
    assert_eq!(b.lock(200, "").await, 1);
    b.send(b"*IDN?").await;
    assert_eq!(b.read_sync().await.message_type, MessageType::DataEnd);
}

#[tokio::test]
async fn a_disconnect_releases_the_lock() {
    let addr = start_server().await;
    let mut b = Session::open(addr, "hislip0").await;
    {
        let mut a = Session::open(addr, "hislip0").await;
        assert_eq!(a.lock(1000, "").await, 1);
        assert_eq!(b.lock(100, "").await, 0, "held while A is alive");
    }
    // A's sockets are closed; the server notices and frees what it held. The
    // wait is the server's, not ours: the request blocks until the release.
    assert_eq!(b.lock(2000, "").await, 1);
}

// -------------------------------------------------------------- device clear

#[tokio::test]
async fn device_clear_abandons_an_in_flight_reply() {
    let clears = Arc::new(AtomicU32::new(0));
    let seen = clears.clone();
    let addr = start_server_with(move |_| {
        let mut dev = TestDevice::new().taking(Duration::from_millis(400));
        dev.clears = clears.clone();
        let dev: Arc<dyn Device> = Arc::new(dev);
        Some(dev)
    })
    .await;
    let mut session = Session::open(addr, "hislip0").await;

    let id = session.send(b"*IDN?").await;
    // Let the server get as far as the bus before clearing.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ack = session.device_clear().await;
    assert_eq!(ack.message_type, MessageType::AsyncDeviceClearAcknowledge);
    assert_eq!(seen.load(Ordering::SeqCst), 1, "clear reached the device");

    let resp = session.read_sync().await;
    assert_eq!(
        resp.message_type,
        MessageType::Interrupted,
        "the abandoned reply is announced, not delivered"
    );
    assert_eq!(resp.message_parameter, id, "which message was abandoned");
    assert_eq!(resp.control_code, 0);
    assert!(resp.payload.is_empty());

    // Finish the handshake and check the session still works.
    MessageType::DeviceClearComplete
        .message_params(0, 0)
        .no_payload()
        .write_to(&mut session.sync)
        .await
        .unwrap();
    session.sync.flush().await.unwrap();
    assert_eq!(
        session.read_sync().await.message_type,
        MessageType::DeviceClearAcknowledge
    );
    session.send(b"*IDN?").await;
    assert_eq!(
        session.read_sync().await.message_type,
        MessageType::DataEnd,
        "the session survives the clear"
    );
}

#[tokio::test]
async fn a_reply_that_beat_the_device_clear_is_still_delivered() {
    let addr = start_server().await;
    let mut session = Session::open(addr, "hislip0").await;

    // Nothing in flight: the reply is already on the wire, so the clear must
    // not turn it into an Interrupted.
    let id = session.send(b"*IDN?").await;
    let resp = session.read_sync().await;
    assert_eq!(resp.message_type, MessageType::DataEnd);
    assert_eq!(resp.message_parameter, id);
    assert_eq!(
        session.device_clear().await.message_type,
        MessageType::AsyncDeviceClearAcknowledge
    );
}

// ------------------------------------------------------------------ chunking

#[tokio::test]
async fn responses_fit_the_maximum_the_client_declared() {
    let big: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let expected = big.clone();
    let addr = start_server_with(move |_| {
        let dev: Arc<dyn Device> = Arc::new(TestDevice::new().replying(big.clone()));
        Some(dev)
    })
    .await;
    let mut session = Session::open(addr, "hislip0").await;

    const LIMIT: u64 = 1024;
    session.declare_max_message_size(LIMIT).await;
    let id = session.send(b"*IDN?").await;

    let mut assembled = Vec::new();
    let mut messages = 0;
    loop {
        let msg = session.read_sync().await;
        assert!(
            matches!(msg.message_type, MessageType::Data | MessageType::DataEnd),
            "unexpected {:?}",
            msg.message_type
        );
        assert_eq!(msg.message_parameter, id);
        let on_the_wire = msg.payload.len() + Message::MESSAGE_HEADER_SIZE;
        assert!(
            on_the_wire as u64 <= LIMIT,
            "sent {on_the_wire} bytes against a {LIMIT} byte limit"
        );
        assembled.extend_from_slice(&msg.payload);
        messages += 1;
        if msg.message_type == MessageType::DataEnd {
            break;
        }
    }
    assert!(messages > 1, "a 4 KiB reply must be split");
    assert_eq!(assembled, expected, "the split reply reassembles");
}

#[tokio::test]
async fn an_empty_reply_still_gets_a_data_end() {
    let addr = start_server_with(|_| {
        let dev: Arc<dyn Device> = Arc::new(TestDevice::new().replying(Vec::new()));
        Some(dev)
    })
    .await;
    let mut session = Session::open(addr, "hislip0").await;

    // A device that answers with an empty message still owes the client a
    // DataEND; silence would leave it blocked until its own timeout.
    let id = session.send(b"*IDN?").await;
    let resp = session.read_sync().await;
    assert_eq!(resp.message_type, MessageType::DataEnd);
    assert_eq!(resp.message_parameter, id);
    assert!(resp.payload.is_empty());
}

#[tokio::test]
async fn a_request_split_across_messages_is_reassembled() {
    let addr = start_server().await;
    let mut session = Session::open(addr, "hislip0").await;

    // "*IDN?" arriving as three Data messages and a DataEND.
    let id = 0xffff_ff00;
    for part in [&b"*I"[..], &b"D"[..], &b"N"[..]] {
        MessageType::Data
            .message_params(0, id)
            .with_payload(part.to_vec())
            .write_to(&mut session.sync)
            .await
            .unwrap();
    }
    MessageType::DataEnd
        .message_params(0, id)
        .with_payload(b"?".to_vec())
        .write_to(&mut session.sync)
        .await
        .unwrap();
    session.sync.flush().await.unwrap();

    let resp = session.read_sync().await;
    assert_eq!(resp.message_type, MessageType::DataEnd);
    assert_eq!(resp.payload, b"ECHO,TEST,SN,1.0\n");
}

// -------------------------------------------------------------- remote/local

#[tokio::test]
async fn every_control_code_that_enables_remote_drives_ren() {
    let remote = Arc::new(AtomicI32::new(-1));
    let seen = remote.clone();
    let addr = start_server_with(move |_| {
        let mut dev = TestDevice::new();
        dev.remote = remote.clone();
        let dev: Arc<dyn Device> = Arc::new(dev);
        Some(dev)
    })
    .await;
    let mut session = Session::open(addr, "hislip0").await;

    // Code 1 is `enableRemote` — what viGpibControlREN(VI_GPIB_REN_ASSERT)
    // sends. It was a no-op once, so REN could be dropped with no way to put
    // it back, leaving the instrument in local: correctness aside, an HP
    // 34401A services the bus ~20x slower that way.
    for (code, expected, name) in [
        (0u8, 0, "disableRemote"),
        (1, 1, "enableRemote"),
        (2, 0, "disableAndGTL"),
        (3, 1, "enableAndGotoRemote"),
        (4, 1, "enableAndLockoutLocal"),
        (5, 1, "enableAndGTRLLO"),
        (6, 0, "justGTL"),
    ] {
        seen.store(-1, Ordering::SeqCst);
        let resp = session
            .async_transaction(
                MessageType::AsyncRemoteLocalControl
                    .message_params(code, 0)
                    .no_payload(),
            )
            .await;
        assert_eq!(resp.message_type, MessageType::AsyncRemoteLocalResponse);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            expected,
            "control code {code} ({name}) did not drive REN"
        );
    }

    // An unknown code is refused rather than silently treated as one of these.
    let resp = session
        .async_transaction(
            MessageType::AsyncRemoteLocalControl
                .message_params(9, 0)
                .no_payload(),
        )
        .await;
    assert_eq!(resp.message_type, MessageType::Error);
}

// ---------------------------------------------------------- service requests

/// A device that reports having pulled SRQ during a query — the MAV case,
/// where the daemon's own read is what cleared the condition.
async fn start_server_raising(stb: u8) -> std::net::SocketAddr {
    start_server_with(move |_| {
        let dev: Arc<dyn Device> = Arc::new(TestDevice::new().requesting_service(stb));
        Some(dev)
    })
    .await
}

#[tokio::test]
async fn a_service_request_seen_during_a_query_is_forwarded() {
    let addr = start_server_raising(0x50).await;
    let mut session = Session::open(addr, "hislip0").await;

    session.send(b"*IDN?").await;
    assert_eq!(
        session.read_sync().await.message_type,
        MessageType::DataEnd,
        "the reply goes out first, so a client woken by the service request \
         finds it already waiting"
    );

    let srq = read_msg_within(&mut session.async_ch, Duration::from_secs(2))
        .await
        .expect("no AsyncServiceRequest for a request raised during the query");
    assert_eq!(srq.message_type, MessageType::AsyncServiceRequest);
    assert_eq!(srq.control_code, 0x50, "the status byte is carried through");
}

#[tokio::test]
async fn no_service_request_when_the_instrument_did_not_raise_one() {
    let addr = start_server().await;
    let mut session = Session::open(addr, "hislip0").await;

    session.send(b"*IDN?").await;
    assert_eq!(session.read_sync().await.message_type, MessageType::DataEnd);

    assert!(
        read_msg_within(&mut session.async_ch, Duration::from_millis(300))
            .await
            .is_none(),
        "service request the instrument never asked for"
    );
}

#[tokio::test]
async fn the_status_byte_the_daemon_consumed_is_handed_over_once() {
    let addr = start_server_raising(0x50).await;
    let mut session = Session::open(addr, "hislip0").await;

    session.send(b"*IDN?").await;
    assert_eq!(session.read_sync().await.message_type, MessageType::DataEnd);
    let srq = read_msg_within(&mut session.async_ch, Duration::from_secs(2))
        .await
        .expect("no AsyncServiceRequest");
    assert_eq!(srq.control_code, 0x50);

    // The daemon's poll cleared RQS at the instrument, so a client reading the
    // status byte itself would see nothing. Hand over what we took.
    let status = session
        .async_transaction(
            MessageType::AsyncStatusQuery
                .message_params(0, 0)
                .no_payload(),
        )
        .await;
    assert_eq!(status.message_type, MessageType::AsyncStatusResponse);
    assert_eq!(status.control_code, 0x50, "consumed status byte");

    // Once only: it is a bit that was taken, not a state to be reported for
    // ever. TestDevice polls as 0.
    let status = session
        .async_transaction(
            MessageType::AsyncStatusQuery
                .message_params(0, 0)
                .no_payload(),
        )
        .await;
    assert_eq!(status.control_code, 0x00, "not reported twice");
}

// Suppress unused-warning for the re-exported InitializeParameter (kept for
// API symmetry with InitializeResponseParameter in tests that might grow).
#[allow(dead_code)]
fn _keep_used(_p: InitializeParameter) {}
