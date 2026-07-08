//! Backpressure diagnostic benchmark for the server-to-client data path.
//!
//! Run with: cargo bench --bench backpressure_bench
//!
//! This benchmark starts a real AnyTLS server, connects it to a fast local
//! producer, and reads from the client stream deliberately slowly. It prints
//! producer/consumer progress and process max RSS so backlog-driven memory
//! growth is visible in one sustained run.

use anyhow::{Context, Result};
use anytls_rs::client::Client;
use anytls_rs::padding::PaddingFactory;
use anytls_rs::server::Server;
use anytls_rs::util::tls;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::io::{AsyncWriteExt, Interest};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep, timeout};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::{TlsAcceptor, TlsConnector};

const PASSWORD: &str = "backpressure_bench_password";
const TOTAL_BYTES: usize = 64 * 1024 * 1024;
const PRODUCER_CHUNK_BYTES: usize = 64 * 1024;
const CONSUMER_READ_BYTES: usize = 8 * 1024;
const CONSUMER_DELAY: Duration = Duration::from_millis(2);
const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug)]
struct BackpressureReport {
    elapsed: Duration,
    produced_bytes: u64,
    consumed_bytes: u64,
    peak_backlog_bytes: u64,
    start_max_rss_kib: Option<u64>,
    peak_max_rss_kib: Option<u64>,
    end_max_rss_kib: Option<u64>,
}

impl BackpressureReport {
    fn max_rss_growth_kib(&self) -> Option<u64> {
        Some(
            self.peak_max_rss_kib?
                .saturating_sub(self.start_max_rss_kib?),
        )
    }
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let report = runtime.block_on(run_backpressure_probe())?;

    println!("backpressure benchmark report:");
    println!("  elapsed: {:?}", report.elapsed);
    println!("  produced_bytes: {}", report.produced_bytes);
    println!("  consumed_bytes: {}", report.consumed_bytes);
    println!("  peak_backlog_bytes: {}", report.peak_backlog_bytes);
    println!("  start_max_rss_kib: {:?}", report.start_max_rss_kib);
    println!("  peak_max_rss_kib: {:?}", report.peak_max_rss_kib);
    println!("  end_max_rss_kib: {:?}", report.end_max_rss_kib);
    println!("  max_rss_growth_kib: {:?}", report.max_rss_growth_kib());

    Ok(())
}

async fn run_backpressure_probe() -> Result<BackpressureReport> {
    let server_port = available_tcp_port()?;
    let server_addr = format!("127.0.0.1:{server_port}");
    let server_task = spawn_anytls_server(server_addr.clone())?;
    wait_for_tcp_listener(&server_addr).await?;

    let (target_addr, produced_bytes, producer_task) =
        spawn_fast_producer(TOTAL_BYTES, PRODUCER_CHUNK_BYTES).await?;

    let client = create_client(server_addr).await?;
    let (stream, session) = client
        .create_proxy_stream(("127.0.0.1".to_string(), target_addr.port()))
        .await
        .context("create proxy stream for fast producer")?;

    let start = Instant::now();
    let start_max_rss_kib = max_rss_kib();
    let mut peak_max_rss_kib = start_max_rss_kib;
    let mut peak_backlog_bytes = 0u64;
    let mut consumed_bytes = 0u64;
    let mut next_sample_at = Instant::now() + SAMPLE_INTERVAL;
    let mut buf = vec![0u8; CONSUMER_READ_BYTES];
    let read_deadline = Instant::now() + READ_TIMEOUT;

    while consumed_bytes < TOTAL_BYTES as u64 {
        let now = Instant::now();
        if now >= read_deadline {
            anyhow::bail!(
                "slow consumer timed out after {:?}: consumed {} of {} bytes",
                READ_TIMEOUT,
                consumed_bytes,
                TOTAL_BYTES
            );
        }

        let n = {
            let mut reader = stream.reader().lock().await;
            timeout(
                read_deadline.saturating_duration_since(now),
                reader.read(&mut buf),
            )
            .await
            .context("timed out waiting for client stream data")??
        };

        if n == 0 {
            anyhow::bail!(
                "client stream ended early: consumed {} of {} bytes",
                consumed_bytes,
                TOTAL_BYTES
            );
        }

        consumed_bytes += n as u64;

        if Instant::now() >= next_sample_at {
            let produced = produced_bytes.load(Ordering::Relaxed);
            peak_backlog_bytes = peak_backlog_bytes.max(produced.saturating_sub(consumed_bytes));
            peak_max_rss_kib = max_optional(peak_max_rss_kib, max_rss_kib());
            next_sample_at += SAMPLE_INTERVAL;
        }

        sleep(CONSUMER_DELAY).await;
    }

    stream.close();
    session.close().await?;
    client.stop_session_pool_cleanup().await;

    let produced_result = timeout(Duration::from_secs(5), producer_task)
        .await
        .context("producer task did not finish")?
        .context("producer task panicked")?;
    produced_result?;

    server_task.abort();
    let _ = server_task.await;

    let end_max_rss_kib = max_rss_kib();
    peak_max_rss_kib = max_optional(peak_max_rss_kib, end_max_rss_kib);
    let produced = produced_bytes.load(Ordering::Relaxed);
    peak_backlog_bytes = peak_backlog_bytes.max(produced.saturating_sub(consumed_bytes));

    Ok(BackpressureReport {
        elapsed: start.elapsed(),
        produced_bytes: produced,
        consumed_bytes,
        peak_backlog_bytes,
        start_max_rss_kib,
        peak_max_rss_kib,
        end_max_rss_kib,
    })
}

fn spawn_anytls_server(addr: String) -> Result<JoinHandle<()>> {
    let server_config = tls::create_server_config()?;
    let tls_acceptor = Arc::new(TlsAcceptor::from(server_config));
    let server = Arc::new(Server::new(
        PASSWORD,
        tls_acceptor,
        PaddingFactory::default(),
        None,
    ));

    Ok(tokio::spawn(async move {
        if let Err(error) = server.listen(&addr).await {
            tracing::error!("[BackpressureBench] server error: {}", error);
        }
    }))
}

async fn create_client(server_addr: String) -> Result<Client> {
    let client_config = tls::create_client_config()?;
    let tls_connector = Arc::new(TlsConnector::from(client_config));
    let server_name = ServerName::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST).into());

    Ok(Client::new(
        PASSWORD,
        server_addr,
        server_name,
        tls_connector,
        PaddingFactory::default(),
    ))
}

async fn spawn_fast_producer(
    total_bytes: usize,
    chunk_bytes: usize,
) -> Result<(SocketAddr, Arc<AtomicU64>, JoinHandle<Result<()>>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let produced_bytes = Arc::new(AtomicU64::new(0));
    let produced_for_task = Arc::clone(&produced_bytes);

    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let chunk = vec![0xA5; chunk_bytes];
        let mut remaining = total_bytes;

        while remaining > 0 {
            let write_len = remaining.min(chunk.len());
            socket.write_all(&chunk[..write_len]).await?;
            produced_for_task.fetch_add(write_len as u64, Ordering::Relaxed);
            remaining -= write_len;
        }

        socket.shutdown().await?;
        Ok(())
    });

    Ok((addr, produced_bytes, task))
}

async fn wait_for_tcp_listener(addr: &str) -> Result<()> {
    let deadline = Instant::now() + SETUP_TIMEOUT;

    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                stream.ready(Interest::WRITABLE).await?;
                return Ok(());
            }
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error).context("wait for AnyTLS server listener"),
        }
    }
}

fn available_tcp_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn max_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(target_os = "macos")]
fn max_rss_kib() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return None;
    }

    let usage = unsafe { usage.assume_init() };
    let bytes = u64::try_from(usage.ru_maxrss).ok()?;
    Some(bytes.div_ceil(1024))
}

#[cfg(target_os = "linux")]
fn max_rss_kib() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return None;
    }

    let usage = unsafe { usage.assume_init() };
    u64::try_from(usage.ru_maxrss).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn max_rss_kib() -> Option<u64> {
    None
}
