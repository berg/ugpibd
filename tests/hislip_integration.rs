// SPDX-License-Identifier: GPL-3.0-or-later
//
// End-to-end round-trip test for the HiSLIP server: client-side message
// codec is the same as the server's, so we can drive a real TCP socket
// without a separate client library.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::BufStream;
use tokio::net::{TcpListener, TcpStream};
use ugpibd::hislip::messages::{
    InitializeParameter, InitializeResponseParameter, Message, MessageType,
};
use ugpibd::hislip::protocol::PROTOCOL_2_0;
use ugpibd::hislip::server::{run, Config, Device};

struct EchoDevice;

#[async_trait::async_trait]
impl Device for EchoDevice {
    async fn execute(&self, cmd: &[u8], _expect_response: bool) -> Result<Option<Vec<u8>>> {
        if cmd.eq_ignore_ascii_case(b"*idn?") {
            Ok(Some(b"ECHO,TEST,SN,1.0\n".to_vec()))
        } else {
            Ok(Some(cmd.to_vec()))
        }
    }
    async fn trigger(&self) -> Result<()> {
        Ok(())
    }
    async fn clear(&self) -> Result<()> {
        Ok(())
    }
    async fn set_remote(&self, _remote: bool) -> Result<()> {
        Ok(())
    }
}

async fn start_server() -> Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = run(listener, Config::default(), |_subaddr| {
            let dev: Arc<dyn Device> = Arc::new(EchoDevice);
            Some(dev)
        })
        .await;
    });
    Ok(addr)
}

fn init_param(protocol: u16, vendor: u16) -> u32 {
    ((protocol as u32) << 16) | vendor as u32
}

async fn read_msg(s: &mut BufStream<TcpStream>) -> Message {
    tokio::time::timeout(Duration::from_secs(2), Message::read_from(s, 1024 * 1024))
        .await
        .expect("read timeout")
        .expect("io error")
        .expect("protocol error")
}

#[tokio::test]
async fn hislip_round_trip_idn_query() {
    let addr = start_server().await.unwrap();

    // Sync channel: Initialize -> expect InitializeResponse with session id.
    let mut sync = BufStream::new(TcpStream::connect(addr).await.unwrap());
    MessageType::Initialize
        .message_params(0, init_param(PROTOCOL_2_0.into(), 0x1234))
        .with_payload(b"hislip0".to_vec())
        .write_to(&mut sync)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut sync).await.unwrap();

    let init_resp = read_msg(&mut sync).await;
    assert_eq!(init_resp.message_type, MessageType::InitializeResponse);
    let resp_param = InitializeResponseParameter(init_resp.message_parameter);
    let session_id = resp_param.session_id();

    // Async channel: AsyncInitialize(session_id) -> expect AsyncInitializeResponse.
    let mut async_ch = BufStream::new(TcpStream::connect(addr).await.unwrap());
    MessageType::AsyncInitialize
        .message_params(0, session_id as u32)
        .no_payload()
        .write_to(&mut async_ch)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut async_ch)
        .await
        .unwrap();
    let ainit_resp = read_msg(&mut async_ch).await;
    assert_eq!(
        ainit_resp.message_type,
        MessageType::AsyncInitializeResponse
    );

    // Sync channel: DataEnd("*IDN?") -> expect DataEnd with echoed IDN.
    MessageType::DataEnd
        .message_params(0, 1)
        .with_payload(b"*IDN?".to_vec())
        .write_to(&mut sync)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut sync).await.unwrap();

    let data_resp = read_msg(&mut sync).await;
    assert_eq!(data_resp.message_type, MessageType::DataEnd);
    assert_eq!(data_resp.message_parameter, 1);
    assert_eq!(data_resp.payload, b"ECHO,TEST,SN,1.0\n");
}

#[tokio::test]
async fn hislip_async_lock_noop() {
    let addr = start_server().await.unwrap();

    // Sync init first to register the session.
    let mut sync = BufStream::new(TcpStream::connect(addr).await.unwrap());
    MessageType::Initialize
        .message_params(0, init_param(PROTOCOL_2_0.into(), 0))
        .with_payload(b"hislip0,14".to_vec())
        .write_to(&mut sync)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut sync).await.unwrap();
    let init_resp = read_msg(&mut sync).await;
    let session_id = InitializeResponseParameter(init_resp.message_parameter).session_id();

    let mut async_ch = BufStream::new(TcpStream::connect(addr).await.unwrap());
    MessageType::AsyncInitialize
        .message_params(0, session_id as u32)
        .no_payload()
        .write_to(&mut async_ch)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut async_ch)
        .await
        .unwrap();
    let _ = read_msg(&mut async_ch).await;

    // Request an exclusive lock (control_code != 0).
    MessageType::AsyncLock
        .message_params(1, 5000)
        .with_payload(b"exclusive".to_vec())
        .write_to(&mut async_ch)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut async_ch)
        .await
        .unwrap();

    let resp = read_msg(&mut async_ch).await;
    assert_eq!(resp.message_type, MessageType::AsyncLockResponse);
    // Success = 1
    assert_eq!(resp.control_code, 1);
}

#[tokio::test]
async fn hislip_rejects_unknown_subaddress() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = run(listener, Config::default(), |sub| {
            if sub == "valid" {
                let dev: Arc<dyn Device> = Arc::new(EchoDevice);
                Some(dev)
            } else {
                None
            }
        })
        .await;
    });

    let mut s = BufStream::new(TcpStream::connect(addr).await.unwrap());
    MessageType::Initialize
        .message_params(0, init_param(PROTOCOL_2_0.into(), 0))
        .with_payload(b"nope".to_vec())
        .write_to(&mut s)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut s).await.unwrap();

    let resp = read_msg(&mut s).await;
    assert_eq!(resp.message_type, MessageType::FatalError);
}

// Suppress unused-warning for the re-exported InitializeParameter (kept for
// API symmetry with InitializeResponseParameter in tests that might grow).
#[allow(dead_code)]
fn _keep_used(_p: InitializeParameter) {}
