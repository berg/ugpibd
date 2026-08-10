// SPDX-License-Identifier: GPL-3.0-or-later
//
// End-to-end tests for the VXI-11 core channel: real TCP, the vxi11 client
// driving the server, a scripted mock backend playing the instrument. Test
// names cite the spec rule they pin (VXIbus Specification VXI-11 rev 1.0,
// section B.6).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::net::TcpListener;
use ugpibd::backend::GpibBackend;
use ugpibd::frontend::instrument::Instrument;
use ugpibd::vxi11::client::{Reply, Vxi11Client};
use ugpibd::vxi11::server::{run, Config};
use ugpibd::vxi11::{self, ErrorCode};

/// What the mock observed, for assertions.
#[derive(Debug, Default)]
struct Observed {
    writes: Vec<(u8, Vec<u8>, bool)>,
    triggers: Vec<u8>,
    clears: Vec<u8>,
    remotes: Vec<u8>,
    locals: Vec<u8>,
    /// Every set_timeout value in order — proves per-op override + restore.
    timeouts: Vec<u32>,
    /// eos state at the moment each read ran.
    eos_at_read: Vec<(u8, bool)>,
}

/// A scripted instrument: reads pop from `reads`; an empty script reads as
/// a bus timeout does (no data, no END).
struct MockBackend {
    reads: VecDeque<(Vec<u8>, bool)>,
    stb: u8,
    eos: (u8, bool),
    observed: Arc<Mutex<Observed>>,
}

impl MockBackend {
    fn new(reads: Vec<(Vec<u8>, bool)>, stb: u8) -> (Self, Arc<Mutex<Observed>>) {
        let observed = Arc::new(Mutex::new(Observed::default()));
        (
            Self {
                reads: reads.into(),
                stb,
                eos: (b'\n', false),
                observed: observed.clone(),
            },
            observed,
        )
    }
}

#[async_trait::async_trait]
impl GpibBackend for MockBackend {
    async fn init(&mut self, _my_pad: u8) -> Result<()> {
        Ok(())
    }

    async fn write(&mut self, pad: u8, data: &[u8], send_eoi: bool) -> Result<()> {
        self.observed
            .lock()
            .unwrap()
            .writes
            .push((pad, data.to_vec(), send_eoi));
        Ok(())
    }

    async fn read(&mut self, _pad: u8, max_len: usize) -> Result<(Vec<u8>, bool)> {
        self.observed.lock().unwrap().eos_at_read.push(self.eos);
        // Script exhausted = the instrument had nothing to say before the
        // timeout, which a real backend reports as no data and no END.
        let (mut data, end) = self.reads.pop_front().unwrap_or_default();
        data.truncate(max_len);
        Ok((data, end))
    }

    async fn device_clear(&mut self, pad: u8) -> Result<()> {
        self.observed.lock().unwrap().clears.push(pad);
        Ok(())
    }

    async fn trigger(&mut self, pad: u8) -> Result<()> {
        self.observed.lock().unwrap().triggers.push(pad);
        Ok(())
    }

    async fn ifc(&mut self) -> Result<()> {
        Ok(())
    }

    async fn ren(&mut self, _enable: bool) -> Result<()> {
        Ok(())
    }

    async fn go_to_remote(&mut self, pad: u8) -> Result<()> {
        self.observed.lock().unwrap().remotes.push(pad);
        Ok(())
    }

    async fn go_to_local(&mut self, pad: u8) -> Result<()> {
        self.observed.lock().unwrap().locals.push(pad);
        Ok(())
    }

    async fn local_lockout(&mut self) -> Result<()> {
        Ok(())
    }

    async fn serial_poll(&mut self, _pad: u8) -> Result<u8> {
        Ok(self.stb)
    }

    fn set_eos(&mut self, eos_char: u8, enabled: bool) {
        self.eos = (eos_char, enabled);
    }

    fn eos(&self) -> (u8, bool) {
        self.eos
    }

    fn set_timeout(&mut self, timeout_ms: u32) {
        self.observed.lock().unwrap().timeouts.push(timeout_ms);
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

/// Start a server over the given mock; returns a connected client.
async fn start(backend: MockBackend, config: Config) -> Result<Vxi11Client> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let ctrl: Arc<tokio::sync::Mutex<dyn GpibBackend>> = Arc::new(tokio::sync::Mutex::new(backend));
    tokio::spawn(async move {
        let instrument_for = move |pad: u8| Arc::new(Instrument::new(ctrl.clone(), pad));
        let _ = run(listener, config, instrument_for).await;
    });
    Vxi11Client::connect("127.0.0.1", port).await
}

async fn start_default(backend: MockBackend) -> Result<Vxi11Client> {
    start(backend, Config::default()).await
}

fn ok(code: u32) -> u32 {
    assert_eq!(code, ErrorCode::NoError.as_u32());
    code
}

/// RULE B.6.3: create_link returns a usable lid, maxRecvSize ≥ 1024, and
/// no error; lids are unique across links.
#[tokio::test]
async fn create_link_returns_unique_lids_and_max_recv_size() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![], 0);
    let mut client = start_default(backend).await?;
    let a = client.create_link("gpib0,18").await?;
    let b = client.create_link("gpib0,23").await?;
    ok(a.error);
    ok(b.error);
    assert!(a.max_recv_size >= 1024);
    assert_ne!(a.lid, b.lid);
    Ok(())
}

/// Table B.4: names that parse to nothing this daemon serves are refused
/// with the spec's own codes.
#[tokio::test]
async fn create_link_refuses_bad_device_names_with_spec_codes() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![], 0);
    let mut client = start_default(backend).await?;
    for (name, code) in [
        ("gpib0,31", ErrorCode::InvalidAddress),
        ("gpib0,18,96", ErrorCode::InvalidAddress),
        ("gpib1,18", ErrorCode::DeviceNotAccessible),
        ("bogus", ErrorCode::SyntaxError),
    ] {
        let resp = client.create_link(name).await?;
        assert_eq!(resp.error, code.as_u32(), "{name}");
    }
    Ok(())
}

/// RULE B.6.5: past the link cap, create_link answers 9; destroying links
/// frees capacity again.
#[tokio::test]
async fn link_capacity_is_enforced_and_recovered() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![], 0);
    let config = Config {
        max_links: 2,
        ..Default::default()
    };
    let mut client = start(backend, config).await?;
    let a = client.create_link("gpib0,1").await?;
    let _b = client.create_link("gpib0,2").await?;
    let refused = client.create_link("gpib0,3").await?;
    assert_eq!(refused.error, ErrorCode::OutOfResources.as_u32());
    ok(client.destroy_link(a.lid).await?);
    let again = client.create_link("gpib0,3").await?;
    ok(again.error);
    Ok(())
}

/// §B.2: a connection closing destroys the links created on it.
#[tokio::test]
async fn a_dropped_connection_frees_its_links() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![], 0);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let ctrl: Arc<tokio::sync::Mutex<dyn GpibBackend>> = Arc::new(tokio::sync::Mutex::new(backend));
    let config = Config {
        max_links: 1,
        ..Default::default()
    };
    tokio::spawn(async move {
        let instrument_for = move |pad: u8| Arc::new(Instrument::new(ctrl.clone(), pad));
        let _ = run(listener, config, instrument_for).await;
    });

    let mut first = Vxi11Client::connect("127.0.0.1", port).await?;
    ok(first.create_link("gpib0,5").await?.error);
    drop(first);

    let mut second = Vxi11Client::connect("127.0.0.1", port).await?;
    // Poll until the server has torn down the first connection's links; the
    // TCP close and the cleanup race this connect.
    for attempt in 0.. {
        let resp = second.create_link("gpib0,5").await?;
        if resp.error == ErrorCode::NoError.as_u32() {
            return Ok(());
        }
        assert_eq!(resp.error, ErrorCode::OutOfResources.as_u32());
        assert!(attempt < 50, "first connection's link never freed");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    unreachable!()
}

/// RULES B.6.13/B.6.14: write transfers the bytes, EOI rides the end flag,
/// and size reports what was accepted.
#[tokio::test]
async fn device_write_carries_data_and_end_flag() -> Result<()> {
    let (backend, observed) = MockBackend::new(vec![], 0);
    let mut client = start_default(backend).await?;
    let link = client.create_link("gpib0,18").await?;

    let with_end = client.device_write(link.lid, b"PRINT\r\n", true, 0).await?;
    ok(with_end.error);
    assert_eq!(with_end.size, 7);
    let without_end = client.device_write(link.lid, b"partial", false, 0).await?;
    ok(without_end.error);

    let obs = observed.lock().unwrap();
    assert_eq!(obs.writes[0], (18, b"PRINT\r\n".to_vec(), true));
    assert_eq!(obs.writes[1], (18, b"partial".to_vec(), false));
    Ok(())
}

/// RULE B.6.16: data over maxRecvSize is a parameter error and nothing
/// reaches the device.
#[tokio::test]
async fn an_oversized_write_is_a_parameter_error() -> Result<()> {
    let (backend, observed) = MockBackend::new(vec![], 0);
    let config = Config {
        max_recv_size: 1024,
        ..Default::default()
    };
    let mut client = start(backend, config).await?;
    let link = client.create_link("gpib0,18").await?;
    let resp = client
        .device_write(link.lid, &vec![0u8; 2048], true, 0)
        .await?;
    assert_eq!(resp.error, ErrorCode::ParameterError.as_u32());
    assert_eq!(resp.size, 0);
    assert!(observed.lock().unwrap().writes.is_empty());
    Ok(())
}

/// RULE B.6.23.1.a: an END (EOI) terminated read sets the END reason bit.
#[tokio::test]
async fn a_read_ending_in_eoi_reports_end() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![(b"HP8594E\r\n".to_vec(), true)], 0);
    let mut client = start_default(backend).await?;
    let link = client.create_link("gpib0,18").await?;
    let resp = client.device_read(link.lid, 20480, 0, None).await?;
    ok(resp.error);
    assert_eq!(resp.data, b"HP8594E\r\n");
    assert_eq!(resp.reason, vxi11::RX_END);
    Ok(())
}

/// RULE B.6.23.1.b: exactly requestSize bytes sets REQCNT — and a zero
/// requestSize terminates immediately with REQCNT and no bus traffic.
#[tokio::test]
async fn request_size_termination_reports_reqcnt() -> Result<()> {
    let (backend, observed) = MockBackend::new(vec![(b"1234".to_vec(), false)], 0);
    let mut client = start_default(backend).await?;
    let link = client.create_link("gpib0,18").await?;

    let zero = client.device_read(link.lid, 0, 0, None).await?;
    ok(zero.error);
    assert_eq!(zero.reason, vxi11::RX_REQCNT);
    assert!(zero.data.is_empty());
    assert!(
        observed.lock().unwrap().eos_at_read.is_empty(),
        "no bus read"
    );

    let exact = client.device_read(link.lid, 4, 0, None).await?;
    ok(exact.error);
    assert_eq!(exact.reason, vxi11::RX_REQCNT);
    assert_eq!(exact.data, b"1234");
    Ok(())
}

/// RULE B.6.23.1.c: a termchar-terminated read sets CHR; the terminator is
/// enabled on the bus only for that read, and the prior EOS state comes
/// back afterwards.
#[tokio::test]
async fn a_termchar_read_reports_chr_and_restores_eos() -> Result<()> {
    let (mut backend, observed) = MockBackend::new(vec![(b"line\n".to_vec(), false)], 0);
    // A sibling front-end (Prologix ++eos) configured this; it must survive.
    backend.set_eos(b'X', true);
    let ctrl_state = backend.eos();
    let mut client = start_default(backend).await?;
    let link = client.create_link("gpib0,18").await?;

    let resp = client.device_read(link.lid, 20480, 0, Some(b'\n')).await?;
    ok(resp.error);
    assert_eq!(resp.reason, vxi11::RX_CHR);
    assert_eq!(resp.data, b"line\n");

    // Scoped: clippy refuses a std MutexGuard alive across an await, even
    // one explicitly dropped first.
    {
        let obs = observed.lock().unwrap();
        assert_eq!(
            obs.eos_at_read[0],
            (b'\n', true),
            "termchar active during read"
        );
    }

    // Read again without termchar: the restored state should be what the
    // sibling configured, and the read itself must run with EOS off. The
    // short io_timeout keeps the deliberate-timeout leg fast.
    let resp = client.device_read(link.lid, 20480, 300, None).await?;
    assert_eq!(
        resp.error,
        ErrorCode::IoTimeout.as_u32(),
        "script exhausted"
    );
    let obs = observed.lock().unwrap();
    assert_eq!(
        obs.eos_at_read[1],
        (ctrl_state.0, false),
        "EOS off for plain read"
    );
    Ok(())
}

/// RULE B.6.27: a timeout returns error 15 with whatever arrived and
/// reason 0 — partial data accumulated across poll slices included.
#[tokio::test]
async fn a_timed_out_read_reports_15_with_partial_data() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![(b"HALF".to_vec(), false)], 0);
    let mut client = start_default(backend).await?;
    let link = client.create_link("gpib0,18").await?;
    let resp = client.device_read(link.lid, 20480, 300, None).await?;
    assert_eq!(resp.error, ErrorCode::IoTimeout.as_u32());
    assert_eq!(resp.reason, 0);
    assert_eq!(resp.data, b"HALF");
    Ok(())
}

/// The io_timeout deadline is enforced server-side in short slices — the
/// adapter's coarse timeout table never sees the client's number, so the
/// reply always beats the client's own RPC deadline — and the daemon
/// default is restored afterwards.
#[tokio::test]
async fn io_timeout_is_sliced_and_the_default_restored() -> Result<()> {
    let (backend, observed) = MockBackend::new(vec![(b"x".to_vec(), true)], 0);
    let config = Config {
        default_io_timeout_ms: 3000,
        ..Default::default()
    };
    let mut client = start(backend, config).await?;
    let link = client.create_link("gpib0,18").await?;
    client.device_read(link.lid, 1, 25000, None).await?;
    let obs = observed.lock().unwrap();
    assert_eq!(
        obs.timeouts,
        vec![250, 3000],
        "one slice (data arrived), then the default back"
    );
    Ok(())
}

/// RULES B.6.15/B.6.24/B.6.10: an unknown lid answers 4 everywhere.
#[tokio::test]
async fn an_unknown_lid_is_error_4_everywhere() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![], 0);
    let mut client = start_default(backend).await?;
    let e4 = ErrorCode::InvalidLinkIdentifier.as_u32();
    assert_eq!(client.device_write(99, b"x", true, 0).await?.error, e4);
    assert_eq!(client.device_read(99, 10, 0, None).await?.error, e4);
    assert_eq!(client.device_readstb(99, 0).await?.error, e4);
    assert_eq!(client.device_trigger(99).await?, e4);
    assert_eq!(client.device_clear(99).await?, e4);
    assert_eq!(client.destroy_link(99).await?, e4);
    Ok(())
}

/// RULE B.6.10: a destroyed link is unknown afterwards.
#[tokio::test]
async fn a_destroyed_link_is_gone() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![], 0);
    let mut client = start_default(backend).await?;
    let link = client.create_link("gpib0,18").await?;
    ok(client.destroy_link(link.lid).await?);
    assert_eq!(
        client.destroy_link(link.lid).await?,
        ErrorCode::InvalidLinkIdentifier.as_u32()
    );
    Ok(())
}

/// B.6.5: readstb returns the instrument's status byte.
#[tokio::test]
async fn readstb_returns_the_polled_byte() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![], 0x50);
    let mut client = start_default(backend).await?;
    let link = client.create_link("gpib0,18").await?;
    let resp = client.device_readstb(link.lid, 0).await?;
    ok(resp.error);
    assert_eq!(resp.stb, 0x50);
    Ok(())
}

/// trigger/clear/remote/local reach the device as the corresponding bus
/// operations, addressed to the link's PAD.
#[tokio::test]
async fn generic_operations_reach_the_addressed_device() -> Result<()> {
    let (backend, observed) = MockBackend::new(vec![], 0);
    let mut client = start_default(backend).await?;
    let link = client.create_link("gpib0,7").await?;
    ok(client.device_trigger(link.lid).await?);
    ok(client.device_clear(link.lid).await?);
    ok(client.device_remote(link.lid).await?);
    ok(client.device_local(link.lid).await?);
    let obs = observed.lock().unwrap();
    assert_eq!(
        (
            &obs.triggers[..],
            &obs.clears[..],
            &obs.remotes[..],
            &obs.locals[..]
        ),
        (&[7u8][..], &[7u8][..], &[7u8][..], &[7u8][..])
    );
    Ok(())
}

/// Phase boundaries are honest refusals: lock/enable_srq/intr_chan answer
/// 8, unlock answers 12 (no lock is held — true), docmd answers 8 in its
/// own response shape.
#[tokio::test]
async fn unimplemented_procedures_refuse_honestly() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![], 0);
    let mut client = start_default(backend).await?;
    let link = client.create_link("gpib0,18").await?;

    let mut lock_args = Vec::new();
    ugpibd::vxi11::messages::DeviceLockParms {
        lid: link.lid,
        flags: 0,
        lock_timeout_ms: 0,
    }
    .encode(&mut lock_args);
    let reply = client
        .call(
            vxi11::DEVICE_CORE_PROG,
            vxi11::DEVICE_CORE_VERS,
            vxi11::DEVICE_LOCK,
            &lock_args,
        )
        .await?;
    let Reply::Success(results) = reply else {
        panic!("lock refusal should be a VXI-11 error, not an RPC one");
    };
    assert_eq!(
        ugpibd::vxi11::messages::decode_device_error(&results)?,
        ErrorCode::OperationNotSupported.as_u32()
    );

    let mut unlock_args = Vec::new();
    ugpibd::vxi11::xdr::put_i32(&mut unlock_args, link.lid);
    let reply = client
        .call(
            vxi11::DEVICE_CORE_PROG,
            vxi11::DEVICE_CORE_VERS,
            vxi11::DEVICE_UNLOCK,
            &unlock_args,
        )
        .await?;
    let Reply::Success(results) = reply else {
        panic!("unlock should decode");
    };
    assert_eq!(
        ugpibd::vxi11::messages::decode_device_error(&results)?,
        ErrorCode::NoLockHeldByThisLink.as_u32()
    );
    Ok(())
}

/// RFC 5531: an unknown program is PROG_UNAVAIL, a wrong version
/// PROG_MISMATCH, an unknown procedure PROC_UNAVAIL.
#[tokio::test]
async fn rpc_level_errors_use_rpc_level_replies() -> Result<()> {
    let (backend, _) = MockBackend::new(vec![], 0);
    let mut client = start_default(backend).await?;
    assert_eq!(
        client.call(0x0607FF, 1, 10, &[]).await?,
        Reply::Accepted(1), // PROG_UNAVAIL
    );
    assert_eq!(
        client.call(vxi11::DEVICE_CORE_PROG, 2, 10, &[]).await?,
        Reply::Accepted(2), // PROG_MISMATCH
    );
    assert_eq!(
        client.call(vxi11::DEVICE_CORE_PROG, 1, 999, &[]).await?,
        Reply::Accepted(3), // PROC_UNAVAIL
    );
    Ok(())
}
