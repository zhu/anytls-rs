//! Wire-level UDP regressions using only local UDP sockets. Request and packet
//! addresses deliberately use the two different encodings specified by sing.

use anytls_rs::server::{handle_udp_over_tcp, handle_udp_over_tcp_v1};
use anytls_rs::session::{Stream, StreamReader};
use anytls_rs::util::{AnyTlsError, Result};
use bytes::Bytes;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

async fn bounded<F: Future>(future: F) -> F::Output {
    timeout(Duration::from_secs(5), future)
        .await
        .expect("local UDP operation timed out")
}

struct Proxy {
    input: mpsc::Sender<Bytes>,
    output: mpsc::Receiver<(u32, Bytes)>,
    buffered: Vec<u8>,
    task: JoinHandle<Result<()>>,
}

impl Proxy {
    fn new(version_two: bool) -> Self {
        let (input, receiver) = mpsc::channel(32);
        let (writer, output) = mpsc::channel(32);
        let (stream, _) = Stream::new(1, StreamReader::new(1, receiver), writer);
        let stream = Arc::new(stream);
        let task = tokio::spawn(async move {
            if version_two {
                handle_udp_over_tcp(stream).await
            } else {
                handle_udp_over_tcp_v1(stream).await
            }
        });
        Self {
            input,
            output,
            buffered: Vec::new(),
            task,
        }
    }

    async fn send(&self, wire: &[u8], fragment_size: usize) {
        for fragment in wire.chunks(fragment_size) {
            bounded(self.input.send(Bytes::copy_from_slice(fragment)))
                .await
                .expect("proxy stopped reading input");
        }
    }

    async fn expect(&mut self, expected: &[u8]) {
        while self.buffered.len() < expected.len() {
            let (id, chunk) = bounded(self.output.recv())
                .await
                .expect("proxy stopped writing output");
            assert_eq!(id, 1);
            assert!(!chunk.is_empty(), "unexpected stream FIN");
            self.buffered.extend_from_slice(&chunk);
        }
        assert_eq!(&self.buffered[..expected.len()], expected);
        self.buffered.drain(..expected.len());
    }

    async fn finish(self) -> Result<()> {
        drop(self.input);
        bounded(self.task).await.expect("proxy task panicked")
    }
}

fn ip_address(address: SocketAddr, socks: bool) -> Vec<u8> {
    let mut wire = Vec::new();
    match address {
        SocketAddr::V4(address) => {
            wire.push(if socks { 1 } else { 0 });
            wire.extend_from_slice(&address.ip().octets());
        }
        SocketAddr::V6(address) => {
            wire.push(if socks { 4 } else { 1 });
            wire.extend_from_slice(&address.ip().octets());
        }
    }
    wire.extend_from_slice(&address.port().to_be_bytes());
    wire
}

fn domain_address(domain: &str, port: u16, socks: bool) -> Vec<u8> {
    let mut wire = vec![if socks { 3 } else { 2 }, domain.len() as u8];
    wire.extend_from_slice(domain.as_bytes());
    wire.extend_from_slice(&port.to_be_bytes());
    wire
}

fn packet(address: Option<Vec<u8>>, payload: &[u8]) -> Vec<u8> {
    let mut wire = address.unwrap_or_default();
    wire.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    wire.extend_from_slice(payload);
    wire
}

async fn receive(socket: &UdpSocket, expected: &[u8]) -> SocketAddr {
    let mut buffer = vec![0; u16::MAX as usize];
    let (length, peer) = bounded(socket.recv_from(&mut buffer)).await.unwrap();
    assert_eq!(&buffer[..length], expected);
    peer
}

async fn echo(socket: &UdpSocket, expected: &[u8]) {
    let peer = receive(socket, expected).await;
    assert_eq!(
        bounded(socket.send_to(expected, peer)).await.unwrap(),
        expected.len()
    );
}

async fn ipv6_socket() -> Option<UdpSocket> {
    match UdpSocket::bind("[::1]:0").await {
        Ok(socket) => Some(socket),
        Err(error) => {
            eprintln!("skipping IPv6 UDP case: loopback unavailable: {error}");
            None
        }
    }
}

#[tokio::test]
async fn connected_v2_accepts_fragmented_domain_request_and_empty_datagrams() {
    let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = target.local_addr().unwrap();
    let mut proxy = Proxy::new(true);
    let mut request = vec![1];
    // A domain-form IP exercises the domain parser/resolver without external DNS.
    request.extend(domain_address("127.0.0.1", address.port(), true));
    proxy.send(&request, 1).await;
    for payload in [b"".as_slice(), b"after-empty", b"fragmented"] {
        proxy.send(&packet(None, payload), 1).await;
        echo(&target, payload).await;
        proxy.expect(&packet(None, payload)).await;
    }
    proxy.finish().await.unwrap();
}

async fn unconnected_roundtrip(version_two: bool) {
    let first = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let second = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let first_addr = first.local_addr().unwrap();
    let second_addr = second.local_addr().unwrap();
    let mut proxy = Proxy::new(version_two);
    if version_two {
        let mut request = vec![0];
        // Mode 0 must parse this hint without attempting to resolve/connect it.
        request.extend(domain_address("unused.invalid", 0, true));
        proxy.send(&request, 1).await;
    }

    // Coalesced packets have different destination ports and address types.
    let mut packets = packet(Some(ip_address(first_addr, false)), b"");
    packets.extend(packet(
        Some(domain_address("127.0.0.1", second_addr.port(), false)),
        b"second-destination",
    ));
    proxy.send(&packets, packets.len()).await;
    let first_peer = receive(&first, b"").await;
    let second_peer = receive(&second, b"second-destination").await;
    // Replies can arrive out of request order; each must carry its real source.
    bounded(second.send_to(b"second-response", second_peer))
        .await
        .unwrap();
    proxy
        .expect(&packet(
            Some(ip_address(second_addr, false)),
            b"second-response",
        ))
        .await;
    bounded(first.send_to(b"", first_peer)).await.unwrap();
    proxy
        .expect(&packet(Some(ip_address(first_addr, false)), b""))
        .await;

    let followup = packet(Some(ip_address(first_addr, false)), b"after-empty");
    proxy.send(&followup, 1).await;
    echo(&first, b"after-empty").await;
    proxy.expect(&followup).await;
    proxy.finish().await.unwrap();
}

#[tokio::test]
async fn unconnected_v2_uses_per_packet_destinations_and_sources() {
    unconnected_roundtrip(true).await;
}

#[tokio::test]
async fn legacy_v1_roundtrips_addresses_and_empty_datagrams() {
    unconnected_roundtrip(false).await;
}

#[tokio::test]
async fn connected_v2_filters_datagrams_from_other_sources() {
    let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut proxy = Proxy::new(true);
    let mut request = vec![1];
    request.extend(ip_address(target.local_addr().unwrap(), true));
    request.extend(packet(None, b"request"));
    proxy.send(&request, request.len()).await;
    let peer = receive(&target, b"request").await;
    bounded(other.send_to(b"wrong-source", peer)).await.unwrap();
    assert!(
        timeout(Duration::from_millis(100), proxy.output.recv())
            .await
            .is_err(),
        "connected UoT forwarded an unrelated peer's datagram"
    );
    bounded(target.send_to(b"reply", peer)).await.unwrap();
    proxy.expect(&packet(None, b"reply")).await;
    proxy.finish().await.unwrap();
}

#[tokio::test]
async fn connected_v2_uses_an_ipv6_socket() {
    let Some(target) = ipv6_socket().await else {
        return;
    };
    let mut proxy = Proxy::new(true);
    let mut request = vec![1];
    request.extend(ip_address(target.local_addr().unwrap(), true));
    proxy.send(&request, 1).await;
    for payload in [b"ipv6".as_slice(), b"", b"after-empty"] {
        proxy.send(&packet(None, payload), 1).await;
        echo(&target, payload).await;
        proxy.expect(&packet(None, payload)).await;
    }
    proxy.finish().await.unwrap();
}

#[tokio::test]
async fn unconnected_v2_switches_families_and_preserves_large_datagrams() {
    let Some(ipv6) = ipv6_socket().await else {
        return;
    };
    let ipv4 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut proxy = Proxy::new(true);
    // Mode 0's initial address is not a restriction on subsequent destinations.
    let mut request = vec![0];
    request.extend(ip_address(ipv4.local_addr().unwrap(), true));
    proxy.send(&request, 1).await;

    for (socket, payload) in [
        (&ipv4, b"ipv4-first".to_vec()),
        (&ipv6, vec![0x5a; 65_527]),
        (&ipv4, b"ipv4-again".to_vec()),
    ] {
        let wire = packet(
            Some(ip_address(socket.local_addr().unwrap(), false)),
            &payload,
        );
        if payload.len() > 65_000 {
            // IPv6 source + UoT length + maximum UDP payload exceeds one PSH.
            assert!(wire.len() > u16::MAX as usize);
        }
        proxy.send(&wire, 4093).await;
        echo(socket, &payload).await;
        proxy.expect(&wire).await;
    }
    proxy.finish().await.unwrap();
}

async fn ended_input(version_two: bool, request: &[u8]) -> Result<()> {
    let proxy = Proxy::new(version_two);
    proxy.send(request, 1).await;
    proxy.finish().await
}

fn assert_truncated(result: Result<()>) {
    match result {
        Err(AnyTlsError::Io(error)) => assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof),
        result => panic!("expected truncated UDP framing, got {result:?}"),
    }
}

#[tokio::test]
async fn connected_v2_distinguishes_packet_boundary_eof_from_truncation() {
    let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut request = vec![1];
    request.extend(ip_address(target.local_addr().unwrap(), true));
    ended_input(true, &request).await.unwrap();
    for partial in [&[0][..], &[0, 3, b'x'][..]] {
        let mut wire = request.clone();
        wire.extend_from_slice(partial);
        assert_truncated(ended_input(true, &wire).await);
    }
    assert_truncated(ended_input(true, &[1, 4, 0]).await);
}

#[tokio::test]
async fn unconnected_formats_reject_truncated_addresses_lengths_and_payloads() {
    let address: SocketAddr = "127.0.0.1:53".parse().unwrap();
    let complete = packet(Some(ip_address(address, false)), b"abc");
    for version_two in [true, false] {
        let request = if version_two {
            let mut request = vec![0];
            request.extend(ip_address(address, true));
            request
        } else {
            Vec::new()
        };
        ended_input(version_two, &request).await.unwrap();
        for size in [1, 4, 6, 7, 8, 9, 10] {
            let mut wire = request.clone();
            wire.extend_from_slice(&complete[..size]);
            assert_truncated(ended_input(version_two, &wire).await);
        }
        for partial in [&[2][..], &[2, 3, b'a'][..]] {
            let mut wire = request.clone();
            wire.extend_from_slice(partial);
            assert_truncated(ended_input(version_two, &wire).await);
        }
    }
}
