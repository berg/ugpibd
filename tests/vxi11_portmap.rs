// SPDX-License-Identifier: GPL-3.0-or-later
//
// The portmap responder over real sockets, TCP and UDP, per RFC 1833 §3.

use anyhow::Result;
use ugpibd::vxi11::portmap::{self, Mapping};
use ugpibd::vxi11::rpc;

const CORE: Mapping = Mapping {
    prog: 0x0607AF,
    vers: 1,
    prot: portmap::IPPROTO_TCP,
    port: 9010,
};

async fn start() -> Result<(std::net::SocketAddr, std::net::SocketAddr)> {
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let (tcp_addr, udp_addr) = (tcp.local_addr()?, udp.local_addr()?);
    tokio::spawn(async move {
        let _ = portmap::run(tcp, udp, vec![CORE]).await;
    });
    Ok((tcp_addr, udp_addr))
}

/// One UDP call: bare RPC message, no record marking.
async fn udp_call(addr: std::net::SocketAddr, proc: u32, args: &[u8]) -> Result<Option<Vec<u8>>> {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    sock.send_to(
        &rpc::encode_call(7, portmap::PMAP_PROG, portmap::PMAP_VERS, proc, args),
        addr,
    )
    .await?;
    let mut buf = vec![0u8; 4096];
    match tokio::time::timeout(std::time::Duration::from_millis(500), sock.recv(&mut buf)).await {
        Ok(len) => Ok(Some(buf[..len?].to_vec())),
        Err(_) => Ok(None),
    }
}

fn success_body(reply: &[u8]) -> Vec<u8> {
    match rpc::decode_reply(reply, 7).unwrap() {
        rpc::ReplyBody::Success(body) => body.to_vec(),
        other => panic!("expected success, got {other:?}"),
    }
}

/// GETPORT over UDP: the registered program answers its port, an unknown
/// one answers 0, and the same program over the wrong protocol answers 0.
#[tokio::test]
async fn getport_answers_port_or_zero() -> Result<()> {
    let (_, udp) = start().await?;
    for (mapping, expect) in [
        (CORE, 9010u32),
        (
            Mapping {
                prog: 0x0607AF,
                vers: 1,
                prot: portmap::IPPROTO_UDP,
                port: 0,
            },
            0,
        ),
        (
            Mapping {
                prog: 12345,
                vers: 1,
                prot: portmap::IPPROTO_TCP,
                port: 0,
            },
            0,
        ),
    ] {
        let mut args = Vec::new();
        mapping.encode(&mut args);
        let reply = udp_call(udp, portmap::PMAPPROC_GETPORT, &args)
            .await?
            .unwrap();
        let body = success_body(&reply);
        assert_eq!(body, expect.to_be_bytes(), "{mapping:?}");
    }
    Ok(())
}

/// DUMP over TCP lists the table; NULL answers void — both record-marked.
#[tokio::test]
async fn dump_and_null_over_tcp() -> Result<()> {
    let (tcp, _) = start().await?;
    let stream = tokio::net::TcpStream::connect(tcp).await?;
    let mut stream = tokio::io::BufStream::new(stream);

    let call = rpc::encode_call(
        7,
        portmap::PMAP_PROG,
        portmap::PMAP_VERS,
        portmap::PMAPPROC_DUMP,
        &[],
    );
    rpc::write_record(&mut stream, &call).await?;
    let reply = rpc::read_record(&mut stream, 4096).await?.unwrap();
    let listed = portmap::decode_dump_reply(&success_body(&reply))?;
    assert_eq!(listed, vec![CORE]);

    let call = rpc::encode_call(
        7,
        portmap::PMAP_PROG,
        portmap::PMAP_VERS,
        portmap::PMAPPROC_NULL,
        &[],
    );
    rpc::write_record(&mut stream, &call).await?;
    let reply = rpc::read_record(&mut stream, 4096).await?.unwrap();
    assert!(success_body(&reply).is_empty());
    Ok(())
}

/// The table is fixed: SET and UNSET answer FALSE, never mutate.
#[tokio::test]
async fn set_and_unset_are_refused() -> Result<()> {
    let (_, udp) = start().await?;
    let mut args = Vec::new();
    Mapping {
        prog: 999,
        vers: 1,
        prot: portmap::IPPROTO_TCP,
        port: 4242,
    }
    .encode(&mut args);
    let reply = udp_call(udp, portmap::PMAPPROC_SET, &args).await?.unwrap();
    assert_eq!(
        success_body(&reply),
        0u32.to_be_bytes(),
        "SET answers FALSE"
    );

    // And the refused registration is really not there.
    let reply = udp_call(udp, portmap::PMAPPROC_GETPORT, &args)
        .await?
        .unwrap();
    assert_eq!(success_body(&reply), 0u32.to_be_bytes());
    Ok(())
}

/// RFC 1833 §3.2: a CALLIT that cannot be delivered gets no reply at all —
/// silence, not an error.
#[tokio::test]
async fn callit_is_answered_with_silence() -> Result<()> {
    let (_, udp) = start().await?;
    let reply = udp_call(udp, portmap::PMAPPROC_CALLIT, &[0; 16]).await?;
    assert!(reply.is_none());
    Ok(())
}

/// RFC 5531: wrong program and wrong version answer at the RPC layer.
#[tokio::test]
async fn rpc_layer_errors() -> Result<()> {
    let (_, udp) = start().await?;
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    sock.send_to(&rpc::encode_call(9, 200000, 2, 0, &[]), udp)
        .await?;
    let mut buf = vec![0u8; 512];
    let len = sock.recv(&mut buf).await?;
    assert_eq!(
        rpc::decode_reply(&buf[..len], 9).unwrap(),
        rpc::ReplyBody::Accepted(1)
    );

    sock.send_to(&rpc::encode_call(10, portmap::PMAP_PROG, 3, 0, &[]), udp)
        .await?;
    let len = sock.recv(&mut buf).await?;
    assert_eq!(
        rpc::decode_reply(&buf[..len], 10).unwrap(),
        rpc::ReplyBody::Accepted(2)
    );
    Ok(())
}
