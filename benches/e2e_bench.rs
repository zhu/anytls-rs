//! End-to-end benchmarks with complete handshakes and verified delivery.
//!
//! Run with: cargo bench --bench e2e_bench

use anyhow::{Result, anyhow};
use anytls_rs::client::Client;
use anytls_rs::padding::PaddingFactory;
use anytls_rs::protocol::{Command, Frame};
use anytls_rs::server::Server;
use anytls_rs::session::{Session, Stream};
use anytls_rs::util::{AnyTlsError, StringMap, tls};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::future::Future;
use std::hint::black_box;
use std::sync::Arc;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::{TlsAcceptor, TlsConnector};

async fn checked(operation: impl Future<Output = Result<()>>) {
    timeout(Duration::from_secs(5), operation)
        .await
        .expect("end-to-end benchmark fixture timed out")
        .expect("end-to-end benchmark fixture failed");
}

/// Own all four session workers and consume the server's stream callbacks.
/// The benchmark must exercise the wire, not stop after filling a send queue.
struct ConnectedSessions {
    client: Arc<Session>,
    server: Arc<Session>,
    incoming: mpsc::UnboundedReceiver<Arc<Stream>>,
    workers: JoinSet<anytls_rs::util::Result<()>>,
}

impl ConnectedSessions {
    async fn new() -> Result<Self> {
        let (client_io, server_io) = tokio::io::duplex(65536);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (server_read, server_write) = tokio::io::split(server_io);
        let padding = PaddingFactory::default();
        let client = Arc::new(Session::new_client(
            client_read,
            client_write,
            padding.clone(),
            None,
        ));
        let mut server = Session::new_server(server_read, server_write, padding.clone());
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        server.set_stream_callback(incoming_tx);
        let server = Arc::new(server);
        let mut workers = JoinSet::new();
        for session in [&client, &server] {
            let receive = session.clone();
            workers.spawn(async move { receive.recv_loop().await });
            let process = session.clone();
            workers.spawn(async move { process.process_stream_data().await });
        }
        let mut settings = StringMap::new();
        settings.insert("v", "2");
        settings.insert("padding-md5", padding.md5());
        client
            .write_control_frame(Frame::with_data(
                Command::Settings,
                0,
                Bytes::from(settings.to_bytes()),
            ))
            .await?;
        Ok(Self {
            client,
            server,
            incoming,
            workers,
        })
    }

    async fn open_pair(&mut self) -> Result<(Arc<Stream>, Arc<Stream>)> {
        let (client, synack) = self.client.open_stream().await?;
        let server = self
            .incoming
            .recv()
            .await
            .ok_or_else(|| anyhow!("server stream callback closed"))?;
        assert_eq!(client.id(), server.id());
        self.server
            .write_control_frame(Frame::control(Command::SynAck, server.id()))
            .await?;
        synack.await??;
        Ok((client, server))
    }

    async fn shutdown(mut self) -> Result<()> {
        self.client.close().await?;
        self.server.close().await?;
        while let Some(result) = self.workers.join_next().await {
            match result? {
                Ok(()) | Err(AnyTlsError::SessionClosed) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

async fn send_and_verify(client: &Stream, server: &Stream, data: Bytes) -> Result<()> {
    client.send_data(data.clone()).await?;
    let mut received = vec![0; data.len()];
    server
        .reader()
        .lock()
        .await
        .read_exact(&mut received)
        .await?;
    assert_eq!(received.as_slice(), data.as_ref());
    black_box(received);
    Ok(())
}

fn bench_e2e_stream_open_and_send(c: &mut Criterion) {
    let mut group = c.benchmark_group("e2e_stream_open_and_send");
    for size in [64, 256, 1024, 4096, 16384] {
        group.bench_with_input(
            BenchmarkId::new("open_and_send", size),
            &size,
            |b, &size| {
                b.to_async(tokio::runtime::Runtime::new().unwrap())
                    .iter(|| {
                        checked(async {
                            let mut sessions = ConnectedSessions::new().await?;
                            let (client, server) = sessions.open_pair().await?;
                            send_and_verify(&client, &server, Bytes::from(vec![0x5a; size]))
                                .await?;
                            sessions.shutdown().await
                        })
                    });
            },
        );
    }
    group.finish();
}

fn bench_e2e_multiple_streams_concurrent(c: &mut Criterion) {
    let mut group = c.benchmark_group("e2e_multiple_streams");
    for count in [1usize, 10, 100] {
        group.bench_with_input(
            BenchmarkId::new("concurrent_streams", count),
            &count,
            |b, &count| {
                b.to_async(tokio::runtime::Runtime::new().unwrap())
                    .iter(|| {
                        checked(async {
                            let mut sessions = ConnectedSessions::new().await?;
                            let mut transfers = JoinSet::new();
                            for _ in 0..count {
                                let (client, server) = sessions.open_pair().await?;
                                transfers.spawn(async move {
                                    send_and_verify(&client, &server, Bytes::from(vec![0x5a; 1024]))
                                        .await
                                });
                            }
                            while let Some(result) = transfers.join_next().await {
                                result??;
                            }
                            sessions.shutdown().await
                        })
                    });
            },
        );
    }
    group.finish();
}

fn bench_e2e_data_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("e2e_data_throughput");
    for (size, iterations) in [(64, 1000), (256, 500), (1024, 200), (4096, 50)] {
        group.throughput(Throughput::Bytes((size * iterations) as u64));
        group.bench_with_input(
            BenchmarkId::new("throughput", size),
            &(size, iterations),
            |b, &(size, iterations)| {
                b.to_async(tokio::runtime::Runtime::new().unwrap())
                    .iter(|| {
                        checked(async {
                            let mut sessions = ConnectedSessions::new().await?;
                            let (client, server) = sessions.open_pair().await?;
                            let payload = Bytes::from(vec![0x5a; size]);
                            let send = async {
                                for _ in 0..iterations {
                                    client.send_data(payload.clone()).await?;
                                }
                                Ok::<_, anyhow::Error>(())
                            };
                            let receive = async {
                                let mut received = vec![0; size * iterations];
                                server
                                    .reader()
                                    .lock()
                                    .await
                                    .read_exact(&mut received)
                                    .await?;
                                assert!(received.iter().all(|byte| *byte == 0x5a));
                                black_box(received);
                                Ok::<_, anyhow::Error>(())
                            };
                            tokio::try_join!(send, receive)?;
                            sessions.shutdown().await
                        })
                    });
            },
        );
    }
    group.finish();
}

fn bench_e2e_session_startup_and_streams(c: &mut Criterion) {
    c.bench_function("e2e_session_startup_and_streams", |b| {
        b.to_async(tokio::runtime::Runtime::new().unwrap())
            .iter(|| {
                checked(async {
                    let mut sessions = ConnectedSessions::new().await?;
                    let (client, server) = sessions.open_pair().await?;
                    send_and_verify(&client, &server, Bytes::from(vec![0x5a; 1024])).await?;
                    sessions.shutdown().await
                })
            });
    });
}

/// Retain the real TLS/authenticated Rust server and UDP socket roundtrip, but
/// own the stream/session directly so no local UDP proxy task leaks per sample.
async fn udp_over_tcp_roundtrip(iterations: usize, payload_size: usize) -> Result<()> {
    let password = "bench_password";
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?.to_string();
    let server = Arc::new(Server::new(
        password,
        Arc::new(TlsAcceptor::from(tls::create_server_config()?)),
        PaddingFactory::default(),
        None,
    ));
    let mut workers = JoinSet::new();
    workers.spawn(async move { server.serve(listener).await.map_err(anyhow::Error::from) });
    let echo = UdpSocket::bind("127.0.0.1:0").await?;
    let echo_addr = echo.local_addr()?;
    workers.spawn(async move {
        let mut buffer = vec![0; payload_size];
        for _ in 0..iterations {
            let (len, peer) = echo.recv_from(&mut buffer).await?;
            assert_eq!(len, payload_size);
            echo.send_to(&buffer[..len], peer).await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let client = Client::new(
        password,
        server_addr,
        ServerName::try_from("localhost")?,
        Arc::new(TlsConnector::from(tls::create_client_config()?)),
        PaddingFactory::default(),
    );
    let (stream, session) = client
        .create_proxy_stream(("sp.v2.udp-over-tcp.arpa".to_string(), 0))
        .await?;
    let mut request = vec![1, 1, 127, 0, 0, 1];
    request.extend_from_slice(&echo_addr.port().to_be_bytes());
    stream.send_data(Bytes::from(request)).await?;
    let payload = vec![0x5a; payload_size];
    let mut packet = (payload_size as u16).to_be_bytes().to_vec();
    packet.extend_from_slice(&payload);
    for _ in 0..iterations {
        stream.send_data(Bytes::copy_from_slice(&packet)).await?;
        let mut reader = stream.reader().lock().await;
        let mut length = [0; 2];
        reader.read_exact(&mut length).await?;
        assert_eq!(u16::from_be_bytes(length) as usize, payload_size);
        let mut received = vec![0; payload_size];
        reader.read_exact(&mut received).await?;
        assert_eq!(received, payload);
        black_box(received);
    }
    stream.close();
    session.close().await?;
    client.stop_session_pool_cleanup().await;
    workers.abort_all();
    while let Some(result) = workers.join_next().await {
        match result {
            Ok(result) => result?,
            Err(error) if error.is_cancelled() => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn bench_udp_over_tcp_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("udp_over_tcp_roundtrip");
    for (size, iterations) in [(64usize, 5usize), (256, 5), (1024, 3)] {
        group.bench_with_input(
            BenchmarkId::new("roundtrip", size),
            &(size, iterations),
            |b, &(size, iterations)| {
                b.to_async(tokio::runtime::Runtime::new().unwrap())
                    .iter(|| checked(udp_over_tcp_roundtrip(iterations, size)));
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_e2e_stream_open_and_send,
    bench_e2e_multiple_streams_concurrent,
    bench_e2e_data_throughput,
    bench_e2e_session_startup_and_streams,
    bench_udp_over_tcp_roundtrip
);
criterion_main!(benches);
