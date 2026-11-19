//! Stream benchmarks with active, bounded producers and consumers.
//!
//! Run with: cargo bench --bench stream_bench

use anytls_rs::session::{Stream, StreamReader};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

fn create_test_stream() -> (Stream, mpsc::Sender<Bytes>, mpsc::Receiver<(u32, Bytes)>) {
    let (writer_tx, writer_rx) = mpsc::channel(64);
    let (reader_tx, reader_rx) = mpsc::channel(64);
    let (stream, _) = Stream::new(1, StreamReader::new(1, reader_rx), writer_tx);
    (stream, reader_tx, writer_rx)
}

fn bench_stream_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_write");
    for size in [64, 256, 1024, 4096, 16384] {
        group.bench_with_input(BenchmarkId::new("write", size), &size, |b, &size| {
            b.to_async(tokio::runtime::Runtime::new().unwrap())
                .iter(|| async {
                    let (stream, _reader_tx, mut writer_rx) = create_test_stream();
                    stream.send_data(Bytes::from(vec![0; size])).await.unwrap();
                    let (id, data) = writer_rx.recv().await.unwrap();
                    assert_eq!(id, stream.id());
                    assert_eq!(data.len(), size);
                    black_box(data);
                })
        });
    }
    group.finish();
}

fn bench_stream_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_read");
    for size in [64, 256, 1024, 4096, 16384] {
        group.bench_with_input(BenchmarkId::new("read", size), &size, |b, &size| {
            b.to_async(tokio::runtime::Runtime::new().unwrap())
                .iter(|| async {
                    let (mut stream, reader_tx, _writer_rx) = create_test_stream();
                    reader_tx.send(Bytes::from(vec![0; size])).await.unwrap();
                    let mut buffer = vec![0; size];
                    stream.read_exact(&mut buffer).await.unwrap();
                    black_box(buffer);
                })
        });
    }
    group.finish();
}

fn bench_streamreader_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("streamreader_read");
    for size in [64, 256, 1024, 4096, 16384] {
        group.bench_with_input(BenchmarkId::new("read", size), &size, |b, &size| {
            b.to_async(tokio::runtime::Runtime::new().unwrap())
                .iter(|| async {
                    let (tx, rx) = mpsc::channel(1);
                    let mut reader = StreamReader::new(1, rx);
                    tx.send(Bytes::from(vec![0; size])).await.unwrap();
                    let mut buffer = vec![0; size];
                    reader.read_exact(&mut buffer).await.unwrap();
                    black_box(buffer);
                })
        });
    }
    group.finish();
}

fn bench_stream_concurrent_read_write(c: &mut Criterion) {
    c.bench_function("stream_concurrent_read_write", |b| {
        b.to_async(tokio::runtime::Runtime::new().unwrap())
            .iter(|| async {
                timeout(Duration::from_secs(5), async {
                    let (stream, reader_tx, mut writer_rx) = create_test_stream();
                    let payload = Bytes::from(vec![0x5a; 1024]);
                    // Both bounded queues have live consumers. Previously 100
                    // writes filled a 64-slot Session queue with no processor.
                    let write = async {
                        for _ in 0..100 {
                            stream.send_data(payload.clone()).await.unwrap();
                        }
                    };
                    let consume_write = async {
                        for _ in 0..100 {
                            let (id, data) = writer_rx.recv().await.unwrap();
                            assert_eq!(id, stream.id());
                            assert_eq!(data, payload);
                            black_box(data);
                        }
                    };
                    let produce_read = async {
                        for _ in 0..100 {
                            reader_tx.send(payload.clone()).await.unwrap();
                        }
                    };
                    let read = async {
                        let mut reader = stream.reader().lock().await;
                        let mut buffer = vec![0; payload.len()];
                        for _ in 0..100 {
                            reader.read_exact(&mut buffer).await.unwrap();
                            assert_eq!(buffer.as_slice(), payload.as_ref());
                            black_box(&buffer);
                        }
                    };
                    tokio::join!(write, consume_write, produce_read, read);
                })
                .await
                .expect("concurrent stream benchmark timed out");
            })
    });
}

criterion_group!(
    benches,
    bench_stream_write,
    bench_stream_read,
    bench_streamreader_read,
    bench_stream_concurrent_read_write
);
criterion_main!(benches);
