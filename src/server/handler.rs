//! Server connection handlers

use crate::protocol::{Command, Frame};
use crate::session::{Session, Stream};
use crate::util::{AnyTlsError, Result, configure_tcp_stream, resolve_host_addresses_with_cache};
use bytes::Bytes;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, timeout};

/// Handler trait for processing new streams
pub trait StreamHandler: Send + Sync {
    /// Handle a new stream
    fn handle_stream(
        &self,
        stream: Arc<Stream>,
        session: Arc<crate::session::Session>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;
}

/// Default stream handler that proxies TCP connections
pub struct TcpProxyHandler {
    // Destination will be read from stream
}

impl Default for TcpProxyHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl TcpProxyHandler {
    /// Create a new TCP proxy handler
    pub fn new() -> Self {
        Self {}
    }
}

impl StreamHandler for TcpProxyHandler {
    fn handle_stream(
        &self,
        stream: Arc<Stream>,
        session: Arc<crate::session::Session>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            // FIN is queued behind all successfully queued data, including when
            // the handler is cancelled. Remote FIN/session close suppresses it.
            let _close = CloseStreamOnDrop(stream.clone());
            tokio::select! {
                biased;
                _ = stream.closed() => Ok(()),
                result = handle_proxy_stream(stream.clone(), session) => result,
            }
        })
    }
}

struct CloseStreamOnDrop(Arc<Stream>);

impl Drop for CloseStreamOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UotVersion {
    Legacy,
    V2,
}

fn uot_version(host: &str) -> Option<UotVersion> {
    if host.eq_ignore_ascii_case("sp.v2.udp-over-tcp.arpa") {
        Some(UotVersion::V2)
    } else if host.eq_ignore_ascii_case("sp.udp-over-tcp.arpa") {
        Some(UotVersion::Legacy)
    } else {
        None
    }
}

async fn handle_proxy_stream(stream: Arc<Stream>, session: Arc<Session>) -> Result<()> {
    let stream_id = stream.id();
    let peer_version = session.peer_version();
    let mut acknowledged = false;
    let result = async {
        let destination = timeout(Duration::from_secs(15), read_socks_addr(stream.clone()))
            .await
            .map_err(|_| AnyTlsError::Protocol("SOCKS address timeout (15s)".into()))??;
        tracing::info!(stream_id, host = %destination.addr, port = destination.port, "Proxy destination");
        if let Some(version) = uot_version(&destination.addr) {
            if peer_version >= 2 {
                session.write_control_frame(Frame::control(Command::SynAck, stream_id)).await?;
            }
            acknowledged = true;
            match version {
                UotVersion::Legacy => crate::server::udp_proxy::handle_udp_over_tcp_v1(stream.clone()).await,
                UotVersion::V2 => crate::server::udp_proxy::handle_udp_over_tcp(stream.clone()).await,
            }
        } else {
            // One deadline covers resolution and every fallback attempt.
            let outbound = timeout(Duration::from_secs(15), async {
                let addresses = resolve_host_addresses_with_cache(&destination.addr, destination.port).await?;
                Ok::<_, AnyTlsError>(connect_candidates(addresses, TcpStream::connect).await?)
            }).await.map_err(|_| AnyTlsError::Protocol(format!("Connection timeout (15s) to {}:{}", destination.addr, destination.port)))??;
            configure_tcp_stream(&outbound, &format!("{}:{}", destination.addr, destination.port));
            if peer_version >= 2 {
                session.write_control_frame(Frame::control(Command::SynAck, stream_id)).await?;
            }
            acknowledged = true;
            proxy_tcp_connection_data_forwarding(stream.clone(), outbound, destination).await
        }
    }.await;

    // Parsing and DNS failures need the same v2 failure report as connect
    // failures. After a success report, subsequent relay failures only close.
    if let Err(error) = &result
        && peer_version >= 2
        && !acknowledged
        && !stream.is_closed()
        && !session.is_closed()
    {
        let error_frame =
            Frame::with_data(Command::SynAck, stream_id, Bytes::from(error.to_string()));
        let _ = session.write_control_frame(error_frame).await;
    }
    result
}

/// Start another address after 250ms, or immediately if all current attempts
/// failed. The first success aborts and joins every losing attempt.
async fn connect_candidates<F, Fut>(addresses: Vec<SocketAddr>, connect: F) -> io::Result<TcpStream>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<TcpStream>> + Send + 'static,
{
    let mut addresses = addresses.into_iter();
    let first = addresses
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no destination addresses"))?;
    let mut attempts = JoinSet::new();
    attempts.spawn(connect(first));
    let stagger = Duration::from_millis(250);
    let delay = tokio::time::sleep(stagger);
    tokio::pin!(delay);
    loop {
        tokio::select! {
            result = attempts.join_next() => {
                let error = match result.expect("at least one connect attempt") {
                    Ok(Ok(stream)) => {
                        attempts.shutdown().await;
                        return Ok(stream);
                    }
                    Ok(Err(error)) => error,
                    Err(error) => io::Error::other(error),
                };
                if attempts.is_empty() {
                    if let Some(address) = addresses.next() {
                        attempts.spawn(connect(address));
                        delay.as_mut().reset(Instant::now() + stagger);
                    } else {
                        return Err(error);
                    }
                }
            }
            _ = &mut delay, if addresses.len() > 0 => {
                attempts.spawn(connect(addresses.next().unwrap()));
                delay.as_mut().reset(Instant::now() + stagger);
            }
        }
    }
}

/// SOCKS5 address (similar to Go's M.Socksaddr)
#[derive(Debug, Clone)]
struct SocksAddr {
    addr: String,
    port: u16,
}

/// Read SOCKS5 address format from stream
/// Format: [ATYP (1 byte) | ADDR (variable) | PORT (2 bytes)]
///
/// 新实现：直接使用 StreamReader，无需额外的 Mutex 包装
async fn read_socks_addr(stream: Arc<Stream>) -> Result<SocksAddr> {
    let stream_id = stream.id();
    tracing::debug!(
        "[Proxy] read_socks_addr: Starting to read SOCKS5 address from stream {}",
        stream_id
    );

    // 获取 reader 的引用
    let reader_mutex = stream.reader();

    // Read ATYP byte first
    tracing::trace!(
        "[Proxy] read_socks_addr: Reading ATYP byte from stream {}",
        stream_id
    );
    let mut atyp_buf = [0u8; 1];
    {
        let mut reader = reader_mutex.lock().await;
        reader
            .read_exact(&mut atyp_buf)
            .await
            .map_err(|e| AnyTlsError::Protocol(format!("Failed to read address type: {}", e)))?;
    }
    tracing::trace!(
        "[Proxy] read_socks_addr: Read ATYP={:02x} from stream {}",
        atyp_buf[0],
        stream_id
    );

    let atyp = atyp_buf[0];
    let addr =
        match atyp {
            0x01 => {
                // IPv4: 4 bytes
                tracing::trace!(
                    "[Proxy] read_socks_addr: Reading IPv4 address (stream {})",
                    stream_id
                );
                let mut ip_buf = [0u8; 4];
                {
                    let mut reader = reader_mutex.lock().await;
                    reader.read_exact(&mut ip_buf).await.map_err(|e| {
                        AnyTlsError::Protocol(format!("Failed to read IPv4: {}", e))
                    })?;
                }
                IpAddr::V4(Ipv4Addr::from(ip_buf)).to_string()
            }
            0x03 => {
                // Domain name: [LEN (1 byte) | DOMAIN (LEN bytes)]
                tracing::trace!(
                    "[Proxy] read_socks_addr: Reading domain name (stream {})",
                    stream_id
                );
                let mut len_buf = [0u8; 1];
                {
                    let mut reader = reader_mutex.lock().await;
                    reader.read_exact(&mut len_buf).await.map_err(|e| {
                        AnyTlsError::Protocol(format!("Failed to read domain length: {}", e))
                    })?;
                }

                let domain_len = len_buf[0] as usize;
                tracing::trace!(
                    "[Proxy] read_socks_addr: Domain length={} (stream {})",
                    domain_len,
                    stream_id
                );
                if domain_len == 0 || domain_len > 255 {
                    return Err(AnyTlsError::Protocol("Invalid domain length".to_string()));
                }

                let mut domain_buf = vec![0u8; domain_len];
                {
                    let mut reader = reader_mutex.lock().await;
                    reader.read_exact(&mut domain_buf).await.map_err(|e| {
                        AnyTlsError::Protocol(format!("Failed to read domain: {}", e))
                    })?;
                }

                String::from_utf8(domain_buf)
                    .map_err(|e| AnyTlsError::Protocol(format!("Invalid domain name: {}", e)))?
            }
            0x04 => {
                // IPv6: 16 bytes
                tracing::trace!(
                    "[Proxy] read_socks_addr: Reading IPv6 address (stream {})",
                    stream_id
                );
                let mut ip_buf = [0u8; 16];
                {
                    let mut reader = reader_mutex.lock().await;
                    reader.read_exact(&mut ip_buf).await.map_err(|e| {
                        AnyTlsError::Protocol(format!("Failed to read IPv6: {}", e))
                    })?;
                }
                IpAddr::V6(Ipv6Addr::from(ip_buf)).to_string()
            }
            _ => {
                return Err(AnyTlsError::Protocol(format!(
                    "Unsupported address type: 0x{:02x}",
                    atyp
                )));
            }
        };

    // Read port (2 bytes, big-endian)
    tracing::trace!(
        "[Proxy] read_socks_addr: Reading port (stream {})",
        stream_id
    );
    let mut port_buf = [0u8; 2];
    {
        let mut reader = reader_mutex.lock().await;
        reader
            .read_exact(&mut port_buf)
            .await
            .map_err(|e| AnyTlsError::Protocol(format!("Failed to read port: {}", e)))?;
    }
    let port = u16::from_be_bytes(port_buf);

    tracing::debug!(
        "[Proxy] read_socks_addr: Successfully read address {}:{} from stream {}",
        addr,
        port,
        stream_id
    );
    Ok(SocksAddr { addr, port })
}

/// Relay both directions as scoped futures. Cancelling the handler drops both
/// TCP halves; neither forwarding loop can outlive its stream.
async fn proxy_tcp_connection_data_forwarding(
    stream: Arc<Stream>,
    outbound: TcpStream,
    destination: SocksAddr,
) -> Result<()> {
    let (mut outbound_read, mut outbound_write) = outbound.into_split();
    let mut bytes_to_outbound = 0u64;
    let mut bytes_to_client = 0u64;
    let to_outbound = async {
        let mut reader = stream.reader().lock().await;
        let mut buffer = [0; 8192];
        loop {
            let n = reader.read(&mut buffer).await?;
            if n == 0 {
                return Ok::<_, AnyTlsError>(());
            }
            outbound_write.write_all(&buffer[..n]).await?;
            bytes_to_outbound += n as u64;
        }
    };
    let to_client = async {
        let mut buffer = [0; 8192];
        loop {
            let n = outbound_read.read(&mut buffer).await?;
            if n == 0 {
                return Ok::<_, AnyTlsError>(());
            }
            stream
                .send_data(Bytes::copy_from_slice(&buffer[..n]))
                .await
                .map_err(|_| AnyTlsError::SessionClosed)?;
            bytes_to_client += n as u64;
        }
    };
    let result = tokio::select! {
        result = to_outbound => result,
        result = to_client => result,
    };
    tracing::debug!(stream_id = stream.id(), host = %destination.addr,
        port = destination.port, bytes_to_outbound, bytes_to_client, "Proxy connection closed");
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::StreamReader;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::net::{TcpListener, TcpSocket};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn missing_and_truncated_socks_addresses_report_eof() {
        for bytes in [Bytes::new(), Bytes::from_static(&[1, 127, 0])] {
            let (data_tx, data_rx) = mpsc::channel(1);
            if !bytes.is_empty() {
                data_tx.send(bytes).await.unwrap();
            }
            drop(data_tx);
            let (writer, _receiver) = mpsc::channel(1);
            let (stream, _) = Stream::new(1, StreamReader::new(1, data_rx), writer);
            let error = timeout(Duration::from_secs(1), read_socks_addr(Arc::new(stream)))
                .await
                .unwrap()
                .unwrap_err();
            assert!(error.to_string().contains("Failed to read"));
            assert!(!error.to_string().contains("Unsupported address type"));
        }
    }

    #[test]
    fn udp_magic_names_require_exact_hosts() {
        assert_eq!(uot_version("sp.v2.udp-over-tcp.arpa"), Some(UotVersion::V2));
        assert_eq!(
            uot_version("SP.UDP-OVER-TCP.ARPA"),
            Some(UotVersion::Legacy)
        );
        for host in [
            "udp-over-tcp.arpa",
            "sp.v2.udp-over-tcp.arpa.example",
            "prefix-sp.v2.udp-over-tcp.arpa",
        ] {
            assert_eq!(uot_version(host), None);
        }
    }

    #[tokio::test]
    async fn connection_tries_next_address_after_refusal() {
        let refused = TcpSocket::new_v4().unwrap();
        refused.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let stream = timeout(
            Duration::from_secs(2),
            connect_candidates(
                vec![refused.local_addr().unwrap(), address],
                TcpStream::connect,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), address);
    }

    #[tokio::test]
    async fn stalled_first_address_does_not_block_later_address() {
        struct Cancelled(Arc<AtomicBool>);
        impl Drop for Cancelled {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let stalled: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stream = timeout(
            Duration::from_secs(2),
            connect_candidates(vec![stalled, address], |candidate| {
                let cancelled = cancelled.clone();
                async move {
                    if candidate == stalled {
                        let _cancelled = Cancelled(cancelled);
                        std::future::pending().await
                    } else {
                        TcpStream::connect(candidate).await
                    }
                }
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), address);
        assert!(
            cancelled.load(Ordering::SeqCst),
            "losing connect attempt must be joined"
        );
    }
}
