// SPDX-License-Identifier: GPL-3.0-or-later
//
// End-to-end tests for the HiSLIP server: the client-side message codec is the
// same as the server's, so we can drive real TCP sockets without a separate
// client library.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::{TcpListener, TcpStream};
use ugpibd::hislip::messages::{
    InitializeParameter, InitializeResponseParameter, Message, MessageType,
};
use ugpibd::hislip::protocol::PROTOCOL_2_0;
use ugpibd::hislip::server::{run, Config, Device, Execution, Modes};

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
    /// Remote/local operations the server asked for, in order.
    ren_ops: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl TestDevice {
    fn new() -> Self {
        Self {
            reply: b"ECHO,TEST,SN,1.0\n".to_vec(),
            delay: Duration::ZERO,
            resource: "gpib0".to_string(),
            service_request: None,
            clears: Arc::new(AtomicU32::new(0)),
            ren_ops: Arc::new(std::sync::Mutex::new(Vec::new())),
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
        self.ren_ops
            .lock()
            .unwrap()
            .push(if remote { "ren(true)" } else { "ren(false)" });
        Ok(())
    }
    async fn go_to_remote(&self) -> Result<()> {
        self.ren_ops.lock().unwrap().push("goto_remote");
        Ok(())
    }
    async fn go_to_local(&self) -> Result<()> {
        self.ren_ops.lock().unwrap().push("gtl");
        Ok(())
    }
    async fn local_lockout(&self) -> Result<()> {
        self.ren_ops.lock().unwrap().push("llo");
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
    start_server_configured(Config::default(), make).await
}

async fn start_server_configured<F>(config: Config, make: F) -> std::net::SocketAddr
where
    F: Fn(&str) -> Option<Arc<dyn Device>> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = run(listener, config, make).await;
    });
    addr
}

/// A server offering `modes`, every sub-address an echoing test device.
async fn start_server_offering(modes: Modes) -> std::net::SocketAddr {
    let config = Config {
        modes,
        ..Default::default()
    };
    start_server_configured(config, |_| {
        let dev: Arc<dyn Device> = Arc::new(TestDevice::new());
        Some(dev)
    })
    .await
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
    /// Control code of the InitializeResponse: bit 0 is the starting mode.
    init_control: u8,
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
        let init_control = resp.control_code;
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
            init_control,
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

    /// Send a command with the RMT-delivered flag set — the client's way of
    /// saying it has consumed the previous response (§6.14.1).
    async fn send_rmt(&mut self, cmd: &[u8]) -> u32 {
        let id = self.message_id;
        self.message_id = self.message_id.wrapping_add(2);
        MessageType::DataEnd
            .message_params(1, id)
            .with_payload(cmd.to_vec())
            .write_to(&mut self.sync)
            .await
            .unwrap();
        self.sync.flush().await.unwrap();
        id
    }

    /// `AsyncStatusQuery` quoting `message_id`, which §6.14.3 requires to be
    /// the most recent Data/DataEND/Trigger id for MAV to be reported.
    async fn status(&mut self, message_id: u32) -> u8 {
        let resp = self
            .async_transaction(
                MessageType::AsyncStatusQuery
                    .message_params(0, message_id)
                    .no_payload(),
            )
            .await;
        assert_eq!(resp.message_type, MessageType::AsyncStatusResponse);
        resp.control_code
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

    /// The whole §6.12 handshake, asking for overlapped mode or not. Returns
    /// the control codes of AsyncDeviceClearAcknowledge (the server's
    /// proposal) and DeviceClearAcknowledge (what was agreed), and restarts
    /// this client's MessageIDs as step 8 requires.
    async fn full_clear(&mut self, request_overlapped: bool) -> (u8, u8) {
        let proposal = self.device_clear().await;
        assert_eq!(
            proposal.message_type,
            MessageType::AsyncDeviceClearAcknowledge
        );
        MessageType::DeviceClearComplete
            .message_params(u8::from(request_overlapped), 0)
            .no_payload()
            .write_to(&mut self.sync)
            .await
            .unwrap();
        self.sync.flush().await.unwrap();
        // Anything still queued from before the clear is the client's to
        // discard (§6.12 step 4).
        let agreed = loop {
            let m = self.read_sync().await;
            if m.message_type == MessageType::DeviceClearAcknowledge {
                break m;
            }
        };
        self.message_id = 0xffff_ff00;
        (proposal.control_code, agreed.control_code)
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
async fn locked_out_traffic_waits_rather_than_being_refused() {
    let addr = start_server().await;
    let mut a = Session::open(addr, "hislip0").await;
    let mut b = Session::open(addr, "hislip0").await;

    // Both work before anybody locks.
    b.send(b"*IDN?").await;
    assert_eq!(b.read_sync().await.message_type, MessageType::DataEnd);

    assert_eq!(a.lock(1000, "").await, 1);

    // §2.6.1: B's synchronous traffic is left unprocessed while A holds the
    // lock. No reply, and in particular no synthesised "resource locked"
    // Error — HiSLIP has no such message, and answering one would show the
    // client a hard failure where the spec calls for a wait.
    b.send(b"*IDN?").await;
    assert!(
        read_msg_within(&mut b.sync, Duration::from_millis(300))
            .await
            .is_none(),
        "locked-out traffic was answered instead of left unprocessed"
    );

    // The holder is unaffected meanwhile.
    a.send(b"*IDN?").await;
    assert_eq!(a.read_sync().await.message_type, MessageType::DataEnd);

    // Releasing the lock lets the waiting message through, rather than having
    // discarded it.
    assert_eq!(a.unlock().await, 1);
    let resp = read_msg_within(&mut b.sync, Duration::from_secs(2))
        .await
        .expect("waiting message was never serviced after the lock freed");
    assert_eq!(resp.message_type, MessageType::DataEnd);
}

#[tokio::test]
async fn a_lock_holder_does_not_block_the_async_channel_of_others() {
    let addr = start_server().await;
    let mut a = Session::open(addr, "hislip0").await;
    let mut b = Session::open(addr, "hislip0").await;

    assert_eq!(a.lock(1000, "").await, 1);

    // §2.6.1 and §6.6: these have to complete for a client holding no lock.
    let _ = b.status(0xffff_fefe).await;
    b.declare_max_message_size(1 << 20).await;
    assert_eq!(b.lock_info().await, (1, 1));
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
async fn device_clear_does_not_send_interrupted() {
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
    // Let the server get as far as the bus before clearing, so the reply is
    // genuinely in flight.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ack = session.device_clear().await;
    assert_eq!(ack.message_type, MessageType::AsyncDeviceClearAcknowledge);
    assert_eq!(seen.load(Ordering::SeqCst), 1, "clear reached the device");

    // Interrupted belongs to the interrupted protocol error (§6.11, §3.1.1),
    // not to device clear (§6.12), whose step 4 puts the discarding on the
    // client. Sending one alone is actively harmful: §3.1.2 rule 4 stops a
    // conformant client from sending anything further until it also sees
    // AsyncInterrupted, which this transaction never produces.
    let resp = session.read_sync().await;
    assert_eq!(
        resp.message_type,
        MessageType::DataEnd,
        "device clear must not answer with Interrupted"
    );
    assert_eq!(resp.message_parameter, id);

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
async fn remote_local_control_codes_map_to_the_right_bus_operations() {
    let ops: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = ops.clone();
    let addr = start_server_with(move |_| {
        let mut dev = TestDevice::new();
        dev.ren_ops = ops.clone();
        let dev: Arc<dyn Device> = Arc::new(dev);
        Some(dev)
    })
    .await;
    let mut session = Session::open(addr, "hislip0").await;

    // Asserting REN only permits remote; a device enters it on being addressed,
    // which is why 3 and 5 do more than 1. GTL is addressed, so 2 and 6 touch
    // this instrument alone. LLO is universal — the standard has no per-device
    // form.
    for (code, expected, name) in [
        (0u8, vec!["ren(false)"], "disableRemote"),
        (1, vec!["ren(true)"], "enableRemote"),
        (2, vec!["gtl", "ren(false)"], "disableAndGTL"),
        (3, vec!["goto_remote"], "enableAndGotoRemote"),
        (4, vec!["ren(true)", "llo"], "enableAndLockoutLocal"),
        (5, vec!["goto_remote", "llo"], "enableAndGTRLLO"),
        (6, vec!["gtl"], "justGTL"),
    ] {
        recorded.lock().unwrap().clear();
        let resp = session
            .async_transaction(
                MessageType::AsyncRemoteLocalControl
                    .message_params(code, 0)
                    .no_payload(),
            )
            .await;
        assert_eq!(resp.message_type, MessageType::AsyncRemoteLocalResponse);
        assert_eq!(
            *recorded.lock().unwrap(),
            expected,
            "control code {code} ({name})"
        );
    }

    // An unknown code is refused rather than silently treated as one of these,
    // and touches the bus not at all.
    recorded.lock().unwrap().clear();
    let resp = session
        .async_transaction(
            MessageType::AsyncRemoteLocalControl
                .message_params(9, 0)
                .no_payload(),
        )
        .await;
    assert_eq!(resp.message_type, MessageType::Error);
    assert!(recorded.lock().unwrap().is_empty());
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
async fn mav_follows_message_flow_with_no_service_request_mask_in_sight() {
    // No *SRE anywhere in this test, and the device raises no service request:
    // MAV is defined by message flow alone (§6.14.1), so a client that polls
    // the status byte without enabling service requests must still see that a
    // reply is waiting.
    let addr = start_server().await;
    let mut session = Session::open(addr, "hislip0").await;

    // Before anything is sent, §6.14 says a status query quotes the first id
    // minus two, and there is nothing available.
    assert_eq!(
        session.status(0xffff_fefe).await & 0x10,
        0,
        "MAV before any I/O"
    );

    let id = session.send(b"*IDN?").await;
    assert_eq!(session.read_sync().await.message_type, MessageType::DataEnd);
    assert_eq!(
        session.status(id).await & 0x10,
        0x10,
        "MAV must be set once a reply has been sent and not consumed"
    );

    // §6.14.3: a query quoting anything but the most recent id reports MAV
    // false, whatever the true state is.
    assert_eq!(
        session.status(0x1234).await & 0x10,
        0,
        "MAV reported for a stale message id"
    );

    // The client says it consumed the response.
    let next = session.send_rmt(b"*CLS").await;
    assert_eq!(
        session.status(next).await & 0x10,
        0,
        "MAV must clear when the client indicates RMT-delivered"
    );
}

#[tokio::test]
async fn a_write_only_command_leaves_mav_clear() {
    let addr = start_server().await;
    let mut session = Session::open(addr, "hislip0").await;

    // No response was sent, so nothing is available to read.
    let id = session.send(b"*CLS").await;
    assert_eq!(session.status(id).await & 0x10, 0);
}

#[tokio::test]
async fn the_status_byte_the_daemon_consumed_is_handed_over_once() {
    let addr = start_server_raising(0x50).await;
    let mut session = Session::open(addr, "hislip0").await;

    let id = session.send(b"*IDN?").await;
    assert_eq!(session.read_sync().await.message_type, MessageType::DataEnd);
    let srq = read_msg_within(&mut session.async_ch, Duration::from_secs(2))
        .await
        .expect("no AsyncServiceRequest");
    assert_eq!(srq.control_code, 0x50);

    // The daemon's poll cleared RQS at the instrument, so hand over what it
    // took. MAV rides along from the server's own tracking.
    assert_eq!(session.status(id).await, 0x50, "consumed status byte");

    // Once only for RQS; MAV persists because the reply is still unconsumed.
    assert_eq!(session.status(id).await, 0x10, "RQS must not be replayed");
}

#[tokio::test]
async fn a_later_command_invalidates_the_consumed_status_byte() {
    let addr = start_server_raising(0x50).await;
    let mut session = Session::open(addr, "hislip0").await;

    session.send(b"*IDN?").await;
    assert_eq!(session.read_sync().await.message_type, MessageType::DataEnd);
    let _ = read_msg_within(&mut session.async_ch, Duration::from_secs(2))
        .await
        .expect("no AsyncServiceRequest");

    // `*CLS` clears the instrument's status registers, so a byte held from
    // before it must not be replayed afterwards. Any command invalidates it,
    // which avoids having to parse the payload to find out which ones matter.
    let id = session.send_rmt(b"*CLS").await;
    assert_eq!(
        session.status(id).await,
        0x00,
        "stale status byte outlived *CLS"
    );
}

#[tokio::test]
async fn a_fatal_error_detected_on_the_async_channel_reaches_both() {
    let addr = start_server().await;
    let mut session = Session::open(addr, "hislip0").await;

    // §6.2 does not care which channel noticed: both get told, then the
    // connection closes. The async channel is the harder direction, because
    // the sync loop is parked in a read that will never return once the
    // client has given up.
    session
        .async_ch
        .write_all(b"XXnot-a-hislip-header-at-all")
        .await
        .unwrap();
    session.async_ch.flush().await.unwrap();

    let on_async = read_msg_within(&mut session.async_ch, Duration::from_secs(2))
        .await
        .expect("no FatalError on the asynchronous channel");
    assert_eq!(on_async.message_type, MessageType::FatalError);

    let on_sync = read_msg_within(&mut session.sync, Duration::from_secs(2))
        .await
        .expect("no FatalError on the synchronous channel");
    assert_eq!(on_sync.message_type, MessageType::FatalError);
}

#[tokio::test]
async fn a_fatal_error_is_reported_on_both_channels() {
    let addr = start_server().await;
    let mut session = Session::open(addr, "hislip0").await;

    // A bad prologue is a framing failure: §6.2 says the server "shall send the
    // FatalError message on the synchronous channel and the asynchronous
    // channel", then close. A client parked on the async channel would
    // otherwise learn nothing.
    session
        .sync
        .write_all(b"XXnot-a-hislip-header-at-all")
        .await
        .unwrap();
    session.sync.flush().await.unwrap();

    let on_sync = read_msg_within(&mut session.sync, Duration::from_secs(2))
        .await
        .expect("no FatalError on the synchronous channel");
    assert_eq!(on_sync.message_type, MessageType::FatalError);

    let on_async = read_msg_within(&mut session.async_ch, Duration::from_secs(2))
        .await
        .expect("no FatalError on the asynchronous channel");
    assert_eq!(on_async.message_type, MessageType::FatalError);
}

// Suppress unused-warning for the re-exported InitializeParameter (kept for
// API symmetry with InitializeResponseParameter in tests that might grow).
#[allow(dead_code)]
fn _keep_used(_p: InitializeParameter) {}

// ------------------------------------------------------------ operating modes
//
// §3: a server supports synchronized, overlapped, or both. The daemon runs
// synchronized only; the other two exist for test harnesses. These pin down
// the negotiation (§6.1, §6.12.1) and what overlapped changes: the server's own
// MessageID sequence (§3.2.1) and MAV by MessageID (§6.14.2).

const OVERLAPPED: u8 = 0x01;
const FIRST_ID: u32 = 0xffff_ff00;
const BEFORE_FIRST_ID: u32 = 0xffff_fefe;

#[test]
fn the_daemon_default_is_synchronized_only() {
    assert_eq!(Config::default().modes, Modes::Synchronized);
}

#[tokio::test]
async fn synchronized_only_never_agrees_to_overlapped() {
    let addr = start_server_offering(Modes::Synchronized).await;
    let mut session = Session::open(addr, "hislip0").await;
    assert_eq!(session.init_control & OVERLAPPED, 0, "starts synchronized");

    let (proposal, agreed) = session.full_clear(true).await;
    assert_eq!(proposal & OVERLAPPED, 0, "proposes synchronized at clear");
    assert_eq!(
        agreed & OVERLAPPED,
        0,
        "a client asking for overlapped is refused by a server without it"
    );

    // Still synchronized: the reply names the request that produced it.
    session.send(b"*CLS").await;
    let id = session.send(b"*IDN?").await;
    assert_eq!(session.read_sync().await.message_parameter, id);
}

#[tokio::test]
async fn prefer_overlapped_starts_overlapped() {
    let addr = start_server_offering(Modes::PreferOverlapped).await;
    let session = Session::open(addr, "hislip0").await;
    assert_eq!(
        session.init_control & OVERLAPPED,
        OVERLAPPED,
        "InitializeResponse bit 0 announces the starting mode"
    );
}

#[tokio::test]
async fn prefer_synchronized_starts_synchronized() {
    let addr = start_server_offering(Modes::PreferSynchronized).await;
    let mut session = Session::open(addr, "hislip0").await;
    assert_eq!(session.init_control & OVERLAPPED, 0);
    session.send(b"*CLS").await;
    let id = session.send(b"*IDN?").await;
    assert_eq!(
        session.read_sync().await.message_parameter,
        id,
        "a session not yet upgraded numbers replies synchronized-style"
    );
}

#[tokio::test]
async fn each_server_proposes_its_preference_at_clear() {
    for (modes, proposed) in [
        (Modes::Synchronized, 0),
        (Modes::PreferSynchronized, 0),
        (Modes::PreferOverlapped, OVERLAPPED),
    ] {
        let addr = start_server_offering(modes).await;
        let mut session = Session::open(addr, "hislip0").await;
        let (proposal, _) = session.full_clear(false).await;
        assert_eq!(proposal & OVERLAPPED, proposed, "{modes:?}");
    }
}

#[tokio::test]
async fn a_server_with_both_modes_grants_whichever_the_client_asks_for() {
    // §6.12.1: "The server shall accept the value proposed by the client in
    // the DeviceClearComplete message if it is capable of supporting them."
    for modes in [Modes::PreferSynchronized, Modes::PreferOverlapped] {
        for request in [false, true] {
            let addr = start_server_offering(modes).await;
            let mut session = Session::open(addr, "hislip0").await;
            let (_, agreed) = session.full_clear(request).await;
            assert_eq!(
                agreed & OVERLAPPED,
                u8::from(request),
                "{modes:?}, client asked for overlapped={request}"
            );
        }
    }
}

#[tokio::test]
async fn the_mode_can_change_at_every_clear() {
    let addr = start_server_offering(Modes::PreferSynchronized).await;
    let mut session = Session::open(addr, "hislip0").await;

    for overlapped in [true, false, true, true, false] {
        let (_, agreed) = session.full_clear(overlapped).await;
        assert_eq!(agreed & OVERLAPPED, u8::from(overlapped));
        // Two sends so the client's and server's sequences diverge, which is
        // what tells the modes apart.
        session.send(b"*CLS").await;
        let id = session.send(b"*IDN?").await;
        let reply = session.read_sync().await.message_parameter;
        if overlapped {
            assert_eq!(reply, FIRST_ID, "overlapped: the server's own first id");
        } else {
            assert_eq!(reply, id, "synchronized: the request's id");
        }
    }
}

#[tokio::test]
async fn overlapped_replies_carry_the_servers_own_sequence() {
    // §3.2.1 rule 2: each Data/DataEND takes the next id from the server's
    // counter, starting at 0xffffff00, regardless of the client's ids.
    let addr = start_server_offering(Modes::PreferOverlapped).await;
    let mut session = Session::open(addr, "hislip0").await;

    // Write-only commands advance the client's counter but not the server's.
    session.send(b"*CLS").await;
    session.send(b"*RST").await;
    session.send(b"*IDN?").await;
    let first = session.read_sync().await;
    assert_eq!(first.message_type, MessageType::DataEnd);
    assert_eq!(first.message_parameter, FIRST_ID);

    session.send(b"*IDN?").await;
    assert_eq!(
        session.read_sync().await.message_parameter,
        FIRST_ID.wrapping_add(2)
    );
}

#[tokio::test]
async fn overlapped_pipelined_queries_are_answered_in_order() {
    // §3: "a series of independent query messages can be sent to the server
    // without regard to when they complete. The responses from each will be
    // returned in the order the queries were sent."
    let addr = start_server_configured(
        Config {
            modes: Modes::PreferOverlapped,
            ..Default::default()
        },
        |_| {
            let dev: Arc<dyn Device> =
                Arc::new(TestDevice::new().taking(Duration::from_millis(20)));
            Some(dev)
        },
    )
    .await;
    let mut session = Session::open(addr, "hislip0").await;

    let queries: Vec<Vec<u8>> = (0..8).map(|i| format!("Q{i}?").into_bytes()).collect();
    for q in &queries {
        session.send(q).await;
    }
    for (i, q) in queries.iter().enumerate() {
        let reply = session.read_sync().await;
        assert_eq!(reply.message_type, MessageType::DataEnd);
        assert_eq!(&reply.payload, q, "reply {i} out of order");
        assert_eq!(reply.message_parameter, FIRST_ID.wrapping_add(2 * i as u32));
    }
}

#[tokio::test]
async fn overlapped_every_chunk_takes_its_own_id() {
    let addr = start_server_offering(Modes::PreferOverlapped).await;
    let mut session = Session::open(addr, "hislip0").await;
    session.declare_max_message_size(16 + 256).await;

    let big = vec![b'x'; 1000];
    let mut cmd = big.clone();
    cmd.push(b'?');
    session.send(&cmd).await;

    let mut expected = FIRST_ID;
    let mut received = Vec::new();
    loop {
        let m = session.read_sync().await;
        assert_eq!(m.message_parameter, expected, "chunk ids run consecutively");
        expected = expected.wrapping_add(2);
        received.extend_from_slice(&m.payload);
        if m.message_type == MessageType::DataEnd {
            break;
        }
        assert_eq!(m.message_type, MessageType::Data);
    }
    assert_eq!(received, cmd);
    assert!(
        expected.wrapping_sub(FIRST_ID) / 2 > 1,
        "the reply was chunked"
    );
}

#[tokio::test]
async fn overlapped_an_empty_reply_takes_an_id_too() {
    let addr = start_server_configured(
        Config {
            modes: Modes::PreferOverlapped,
            ..Default::default()
        },
        |_| {
            let dev: Arc<dyn Device> = Arc::new(TestDevice::new().replying(Vec::new()));
            Some(dev)
        },
    )
    .await;
    let mut session = Session::open(addr, "hislip0").await;
    session.send(b"*IDN?").await;
    let empty = session.read_sync().await;
    assert_eq!(empty.message_type, MessageType::DataEnd);
    assert!(empty.payload.is_empty());
    assert_eq!(empty.message_parameter, FIRST_ID);
    session.send(b"*IDN?").await;
    assert_eq!(
        session.read_sync().await.message_parameter,
        FIRST_ID.wrapping_add(2)
    );
}

#[tokio::test]
async fn overlapped_mav_compares_message_ids() {
    // §6.14.2: MAV is true while any message sent has not been delivered, as
    // told by the MessageID the client quotes (§3.2.2 rule 1).
    let addr = start_server_offering(Modes::PreferOverlapped).await;
    let mut session = Session::open(addr, "hislip0").await;

    assert_eq!(
        session.status(BEFORE_FIRST_ID).await & 0x10,
        0,
        "nothing sent yet"
    );

    session.send(b"A?").await;
    session.send(b"B?").await;
    let a = session.read_sync().await.message_parameter;
    let b = session.read_sync().await.message_parameter;

    assert_eq!(
        session.status(BEFORE_FIRST_ID).await & 0x10,
        0x10,
        "neither reply delivered"
    );
    assert_eq!(session.status(a).await & 0x10, 0x10, "B not delivered");
    assert_eq!(session.status(b).await & 0x10, 0, "everything delivered");
}

#[tokio::test]
async fn overlapped_mav_ignores_rmt_delivered() {
    // RMT-delivered drives MAV only in synchronized mode (§6.14.1). In
    // overlapped mode a flag claiming delivery does not make an undelivered
    // reply vanish.
    let addr = start_server_offering(Modes::PreferOverlapped).await;
    let mut session = Session::open(addr, "hislip0").await;
    session.send(b"A?").await;
    assert_eq!(session.read_sync().await.message_type, MessageType::DataEnd);
    session.send_rmt(b"*CLS").await;
    assert_eq!(session.status(BEFORE_FIRST_ID).await & 0x10, 0x10);
}

#[tokio::test]
async fn overlapped_mav_is_clear_after_write_only_traffic() {
    let addr = start_server_offering(Modes::PreferOverlapped).await;
    let mut session = Session::open(addr, "hislip0").await;
    session.send(b"*CLS").await;
    session.send(b"*RST").await;
    // A round trip on the sync channel so both commands have been processed.
    session.send(b"*IDN?").await;
    let id = session.read_sync().await.message_parameter;
    assert_eq!(session.status(id).await & 0x10, 0);
}

#[tokio::test]
async fn overlapped_message_ids_restart_after_device_clear() {
    // §3.2.1 rule 1.
    let addr = start_server_offering(Modes::PreferOverlapped).await;
    let mut session = Session::open(addr, "hislip0").await;
    for _ in 0..3 {
        session.send(b"*IDN?").await;
        session.read_sync().await;
    }
    let (_, agreed) = session.full_clear(true).await;
    assert_eq!(agreed & OVERLAPPED, OVERLAPPED);

    assert_eq!(
        session.status(BEFORE_FIRST_ID).await & 0x10,
        0,
        "after a clear the client quotes the first id minus two and nothing is pending"
    );
    session.send(b"*IDN?").await;
    assert_eq!(session.read_sync().await.message_parameter, FIRST_ID);
}

#[tokio::test]
async fn overlapped_replies_queued_before_a_clear_keep_their_old_numbers() {
    // Replies already in flight when the clear arrives go out under the old
    // numbering; the new sequence starts only once the handshake completes.
    let addr = start_server_configured(
        Config {
            modes: Modes::PreferOverlapped,
            ..Default::default()
        },
        |_| {
            let dev: Arc<dyn Device> =
                Arc::new(TestDevice::new().taking(Duration::from_millis(100)));
            Some(dev)
        },
    )
    .await;
    let mut session = Session::open(addr, "hislip0").await;
    session.send(b"*IDN?").await;
    session.send(b"*IDN?").await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let proposal = session.device_clear().await;
    assert_eq!(
        proposal.message_type,
        MessageType::AsyncDeviceClearAcknowledge
    );

    MessageType::DeviceClearComplete
        .message_params(OVERLAPPED, 0)
        .no_payload()
        .write_to(&mut session.sync)
        .await
        .unwrap();
    session.sync.flush().await.unwrap();
    let mut before = Vec::new();
    loop {
        let m = session.read_sync().await;
        if m.message_type == MessageType::DeviceClearAcknowledge {
            break;
        }
        before.push(m.message_parameter);
    }
    for (i, id) in before.iter().enumerate() {
        assert_eq!(*id, FIRST_ID.wrapping_add(2 * i as u32));
    }
    session.message_id = FIRST_ID;
    session.send(b"*IDN?").await;
    assert_eq!(session.read_sync().await.message_parameter, FIRST_ID);
}

#[tokio::test]
async fn returning_to_synchronized_restores_the_synchronized_mav_rules() {
    let addr = start_server_offering(Modes::PreferOverlapped).await;
    let mut session = Session::open(addr, "hislip0").await;
    let (_, agreed) = session.full_clear(false).await;
    assert_eq!(agreed & OVERLAPPED, 0);

    let id = session.send(b"*IDN?").await;
    assert_eq!(session.read_sync().await.message_parameter, id);
    assert_eq!(
        session.status(id).await & 0x10,
        0x10,
        "§6.14.1: reply not consumed"
    );
    let next = session.send_rmt(b"*CLS").await;
    assert_eq!(
        session.status(next).await & 0x10,
        0,
        "RMT-delivered clears it"
    );
}

#[tokio::test]
async fn sessions_negotiate_their_modes_independently() {
    // Table 31: the overlapped bit is "related to current connection".
    let addr = start_server_offering(Modes::PreferSynchronized).await;
    let mut upgraded = Session::open(addr, "hislip0").await;
    let mut plain = Session::open(addr, "hislip0").await;
    upgraded.full_clear(true).await;

    upgraded.send(b"*CLS").await;
    upgraded.send(b"*IDN?").await;
    assert_eq!(upgraded.read_sync().await.message_parameter, FIRST_ID);

    plain.send(b"*CLS").await;
    let id = plain.send(b"*IDN?").await;
    assert_eq!(plain.read_sync().await.message_parameter, id);
}
