//! Concurrent connection performance benchmarks for AnyTLS-RS
//!
//! Run with: cargo bench --bench concurrent_bench
//!
//! These benchmarks measure performance under concurrent load

use anytls_rs::padding::PaddingFactory;
use anytls_rs::protocol::{Command, FrameCodec};
use anytls_rs::session::Session;
use bytes::{Bytes, BytesMut};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};
use tokio_util::codec::Decoder;

/// Mock async reader/writer for testing
struct MockStream;

impl MockStream {
    fn new() -> Self {
        Self
    }
}

impl AsyncRead for MockStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for MockStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl Unpin for MockStream {}

async fn create_test_session() -> Session {
    let mock_stream = MockStream::new();
    let (reader, writer) = tokio::io::split(mock_stream);
    let padding = PaddingFactory::default();

    Session::new_client(
        Box::new(reader) as Box<dyn AsyncRead + Send + Unpin>,
        Box::new(writer) as Box<dyn AsyncWrite + Send + Unpin>,
        padding,
        None,
    )
}

fn bench_concurrent_session_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_session_creation");

    for session_count in [1, 5, 10, 20, 50, 100].iter() {
        group.bench_with_input(
            BenchmarkId::new("create_sessions", session_count),
            session_count,
            |b, &count| {
                b.to_async(tokio::runtime::Runtime::new().unwrap())
                    .iter(|| async {
                        let mut sessions = Vec::new();
                        for _ in 0..count {
                            let session = create_test_session().await;
                            sessions.push(Arc::new(session));
                        }
                        black_box(sessions);
                    })
            },
        );
    }

    group.finish();
}

fn bench_concurrent_stream_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_stream_creation");

    for stream_count in [1, 5, 10, 20, 50, 100].iter() {
        group.bench_with_input(
            BenchmarkId::new("create_streams", stream_count),
            stream_count,
            |b, &count| {
                b.to_async(tokio::runtime::Runtime::new().unwrap())
                    .iter(|| async {
                        let session = Arc::new(create_test_session().await);
                        let mut streams = Vec::new();
                        for _ in 0..count {
                            if let Ok((stream, _)) = session.open_stream().await {
                                streams.push(stream);
                            }
                        }
                        black_box(streams);
                    })
            },
        );
    }

    group.finish();
}

async fn receive_push_frames(mut peer: tokio::io::DuplexStream, count: usize, data_size: usize) {
    let mut input = BytesMut::new();
    let mut codec = FrameCodec;
    let mut received = 0;
    while received < count {
        assert_ne!(peer.read_buf(&mut input).await.unwrap(), 0, "early EOF");
        while let Some(frame) = codec.decode(&mut input).unwrap() {
            if frame.cmd == Command::Push {
                assert_eq!(frame.data.len(), data_size);
                received += 1;
            }
        }
    }
    assert_eq!(received, count);
}

fn bench_concurrent_stream_data_send(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_stream_data_send");

    for (stream_count, data_size) in [(5, 1024), (10, 1024), (20, 1024), (50, 1024)].iter() {
        group.bench_with_input(
            BenchmarkId::new(
                "send_data",
                format!("{}streams_{}B", stream_count, data_size),
            ),
            &(stream_count, data_size),
            |b, &(&stream_count, &data_size)| {
                b.to_async(tokio::runtime::Runtime::new().unwrap())
                    .iter(|| async {
                        let (writer, peer) = tokio::io::duplex(64 * 1024);
                        let session = Arc::new(Session::new_client(
                            tokio::io::empty(),
                            writer,
                            PaddingFactory::default(),
                            None,
                        ));
                        let writer_session = session.clone();
                        let writer_task =
                            tokio::spawn(async move { writer_session.process_stream_data().await });

                        let mut streams = Vec::new();
                        for _ in 0..stream_count {
                            let (stream, _) = session.open_stream().await.unwrap();
                            streams.push(stream);
                        }

                        let mut tasks = JoinSet::new();
                        tasks.spawn(receive_push_frames(peer, stream_count * 10, data_size));
                        for stream in streams {
                            tasks.spawn(async move {
                                let payload = Bytes::from(vec![0; data_size]);
                                for _ in 0..10 {
                                    stream.send_data(payload.clone()).await.unwrap();
                                }
                            });
                        }

                        timeout(Duration::from_secs(5), tasks.join_all())
                            .await
                            .expect("timed out waiting for concurrent stream delivery");
                        session.close().await.unwrap();
                        writer_task.await.unwrap().unwrap();
                    })
            },
        );
    }

    group.finish();
}

fn bench_concurrent_multi_session_multi_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_multi_session_multi_stream");

    for (session_count, streams_per_session) in [(2, 5), (5, 10), (10, 10)].iter() {
        group.bench_with_input(
            BenchmarkId::new(
                "multi_session_multi_stream",
                format!("{}s_{}st", session_count, streams_per_session),
            ),
            &(session_count, streams_per_session),
            |b, &(&session_count, &streams_per_session)| {
                b.to_async(tokio::runtime::Runtime::new().unwrap())
                    .iter(|| async {
                        // Create multiple sessions
                        let mut sessions = Vec::new();
                        for _ in 0..session_count {
                            sessions.push(Arc::new(create_test_session().await));
                        }

                        // Create streams for each session
                        let mut all_streams = Vec::new();
                        for session in sessions.iter() {
                            for _ in 0..streams_per_session {
                                if let Ok((stream, _)) = session.open_stream().await {
                                    all_streams.push(stream);
                                }
                            }
                        }

                        black_box((sessions, all_streams));
                    })
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_concurrent_session_creation,
    bench_concurrent_stream_creation,
    bench_concurrent_stream_data_send,
    bench_concurrent_multi_session_multi_stream
);
criterion_main!(benches);
