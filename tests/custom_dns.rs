//! Exercise custom DNS over real loopback UDP/TCP, without public DNS access.

use anytls_rs::util::{AnyTlsError, resolve_host_addresses_with_cache, set_custom_dns_servers};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};

const IPV4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 42);
const IPV6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 42);
const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Requests {
    udp_answers: AtomicUsize,
    udp_truncated: AtomicUsize,
    tcp_answers: AtomicUsize,
}

// Echo the transaction ID and question (including its original letter case).
// The response omits any query EDNS options and contains one A or AAAA answer.
fn answer(query: &[u8], truncate: bool) -> io::Result<Vec<u8>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid test DNS query");
    if query.len() < 17 || query[4..6] != [0, 1] {
        return Err(invalid());
    }
    let mut end = 12;
    loop {
        let length = *query.get(end).ok_or_else(invalid)? as usize;
        end += 1;
        if length == 0 {
            break;
        }
        if length > 63 || end + length > query.len() {
            return Err(invalid());
        }
        end += length;
    }
    let kind_class = query.get(end..end + 4).ok_or_else(invalid)?;
    let data = match u16::from_be_bytes([kind_class[0], kind_class[1]]) {
        1 => IPV4.octets().to_vec(),
        28 => IPV6.octets().to_vec(),
        _ => return Err(invalid()),
    };
    end += 4;
    let mut response = query[..2].to_vec();
    let flags = 0x8480u16 | ((query[2] as u16 & 1) << 8) | if truncate { 0x0200 } else { 0 };
    response.extend_from_slice(&flags.to_be_bytes());
    response.extend_from_slice(&[0, 1, 0, u8::from(!truncate), 0, 0, 0, 0]);
    response.extend_from_slice(&query[12..end]);
    if !truncate {
        response.extend_from_slice(&[0xc0, 0x0c]); // Answer name points to the question.
        response.extend_from_slice(kind_class);
        response.extend_from_slice(&60u32.to_be_bytes());
        response.extend_from_slice(&(data.len() as u16).to_be_bytes());
        response.extend_from_slice(&data);
    }
    Ok(response)
}

async fn serve_tcp(mut stream: TcpStream, requests: Arc<Requests>) -> io::Result<()> {
    loop {
        // DNS over TCP has a two-byte big-endian length before each message.
        let result = timeout(IO_TIMEOUT, async {
            let length = stream.read_u16().await? as usize;
            if !(17..=4096).contains(&length) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "DNS length"));
            }
            let mut query = vec![0; length];
            stream.read_exact(&mut query).await?;
            let response = answer(&query, false)?;
            stream.write_u16(response.len() as u16).await?;
            stream.write_all(&response).await?;
            requests.tcp_answers.fetch_add(1, Ordering::Relaxed);
            Ok::<_, io::Error>(())
        })
        .await?;
        match result {
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            result => result?,
        }
    }
}

#[tokio::test]
async fn custom_dns_keeps_ports_and_retries_truncated_udp_over_tcp() {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    assert_ne!(address.port(), 53);
    let udp = UdpSocket::bind(address).await.unwrap();
    let requests = Arc::new(Requests::default());

    let udp_requests = requests.clone();
    let udp_task = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        loop {
            let (length, peer) = udp.recv_from(&mut buffer).await.unwrap();
            let query = &buffer[..length];
            let truncate = query.get(12) == Some(&12)
                && query
                    .get(13..25)
                    .is_some_and(|label| label.eq_ignore_ascii_case(b"tcp-fallback"));
            let response = answer(query, truncate).unwrap();
            timeout(IO_TIMEOUT, udp.send_to(&response, peer))
                .await
                .unwrap()
                .unwrap();
            let counter = if truncate {
                &udp_requests.udp_truncated
            } else {
                &udp_requests.udp_answers
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    });
    let tcp_requests = requests.clone();
    let tcp_task = tokio::spawn(async move {
        // Drop aborts every connection handler, including persistent DNS TCP connections.
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = tcp.accept() => {
                    let (stream, _) = accepted.unwrap();
                    connections.spawn(serve_tcp(stream, tcp_requests.clone()));
                }
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    result.unwrap().unwrap();
                }
            }
        }
    });

    // Collect results before asserting, so errors/timeouts also reset global DNS.
    let result = timeout(Duration::from_secs(5), async {
        set_custom_dns_servers(&[address.to_string()]).await?;
        let udp = resolve_host_addresses_with_cache("udp-only.custom-dns.test.", 80).await?;
        let cached = resolve_host_addresses_with_cache("udp-only.custom-dns.test.", 443).await?;
        let tcp = resolve_host_addresses_with_cache("tcp-fallback.custom-dns.test.", 8443).await?;
        Ok::<_, AnyTlsError>((udp, cached, tcp))
    })
    .await;
    let reset = timeout(IO_TIMEOUT, set_custom_dns_servers(&[])).await;
    udp_task.abort();
    tcp_task.abort();
    let udp_stopped = timeout(IO_TIMEOUT, udp_task).await.unwrap().unwrap_err();
    let tcp_stopped = timeout(IO_TIMEOUT, tcp_task).await.unwrap().unwrap_err();
    reset.unwrap().unwrap();
    assert!(udp_stopped.is_cancelled(), "UDP DNS task panicked");
    assert!(tcp_stopped.is_cancelled(), "TCP DNS task panicked");

    let (udp, cached, tcp) = result.expect("custom DNS lookup timed out").unwrap();
    for (mut actual, port) in [(udp, 80), (cached, 443), (tcp, 8443)] {
        actual.sort_unstable();
        assert_eq!(
            actual,
            vec![
                SocketAddr::from((IPV4, port)),
                SocketAddr::from((IPV6, port))
            ]
        );
    }
    assert!(requests.udp_answers.load(Ordering::Relaxed) >= 2);
    assert!(requests.udp_truncated.load(Ordering::Relaxed) >= 2);
    assert!(requests.tcp_answers.load(Ordering::Relaxed) >= 2);
}
