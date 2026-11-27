use anytls_rs::padding::PaddingFactory;
use anytls_rs::protocol::{Command, Frame, FrameCodec};
use anytls_rs::session::Session;
use bytes::{Bytes, BytesMut};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, duplex, empty, sink};
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::codec::{Decoder, Encoder};

const DEADLINE: Duration = Duration::from_secs(2);

struct FailingWriter;

impl AsyncWrite for FailingWriter {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test failure",
        )))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct BlockedWriter(Arc<Notify>);

impl AsyncWrite for BlockedWriter {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        self.0.notify_one();
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct InterruptibleShutdown {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<Notify>,
}

impl AsyncWrite for InterruptibleShutdown {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.started.notify_one();
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

#[tokio::test]
async fn concurrent_close_waits_and_cancelled_cleanup_can_be_retried() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let session = Arc::new(Session::new_server(
        empty(),
        InterruptibleShutdown {
            calls: calls.clone(),
            started: started.clone(),
        },
        PaddingFactory::default(),
    ));
    let first = tokio::spawn({
        let session = session.clone();
        async move { session.close().await }
    });
    timeout(DEADLINE, started.notified()).await.unwrap();
    assert!(
        timeout(Duration::from_millis(20), session.close())
            .await
            .is_err(),
        "a concurrent close must wait for in-progress cleanup"
    );
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    timeout(DEADLINE, session.close()).await.unwrap().unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_transport_write_closes_without_locking_itself() {
    let session = Session::new_server(empty(), FailingWriter, PaddingFactory::default());
    let result = timeout(
        DEADLINE,
        session.write_data_frame(1, Bytes::from_static(b"data")),
    )
    .await
    .expect("write error must not deadlock during session close");
    assert!(result.is_err());
    assert!(session.is_closed());
}

#[tokio::test]
async fn closing_session_cancels_pending_transport_read() {
    let (reader, _peer) = duplex(64);
    let session = Arc::new(Session::new_server(
        reader,
        sink(),
        PaddingFactory::default(),
    ));
    let recv = tokio::spawn({
        let session = session.clone();
        async move { session.recv_loop().await }
    });
    tokio::task::yield_now().await;
    session.close().await.unwrap();
    timeout(DEADLINE, recv).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn closing_session_cancels_pending_transport_write() {
    let writing = Arc::new(Notify::new());
    let session = Arc::new(Session::new_server(
        empty(),
        BlockedWriter(writing.clone()),
        PaddingFactory::default(),
    ));
    let write = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .write_data_frame(1, Bytes::from_static(b"data"))
                .await
        }
    });
    timeout(DEADLINE, writing.notified()).await.unwrap();
    timeout(DEADLINE, session.close()).await.unwrap().unwrap();
    assert!(timeout(DEADLINE, write).await.unwrap().unwrap().is_err());
}

#[tokio::test]
async fn processor_started_after_close_observes_durable_notification() {
    let session = Session::new_server(empty(), sink(), PaddingFactory::default());
    session.close().await.unwrap();
    timeout(DEADLINE, session.process_stream_data())
        .await
        .unwrap()
        .unwrap();
    timeout(DEADLINE, session.closed()).await.unwrap();
}

#[tokio::test]
async fn closing_before_processor_starts_releases_stream_writers() {
    let session = Session::new_client(empty(), sink(), PaddingFactory::default(), None);
    let (stream, _) = session.open_stream().await.unwrap();
    session.close().await.unwrap();
    assert!(
        timeout(DEADLINE, stream.send_data(Bytes::from_static(b"late")))
            .await
            .unwrap()
            .is_err()
    );
    assert!(!session.has_active_streams().await);
}

#[tokio::test]
async fn remote_fin_closes_retained_stream_and_preserves_preceding_data() {
    let (reader, mut peer) = duplex(128);
    let session = Arc::new(Session::new_client(
        reader,
        sink(),
        PaddingFactory::default(),
        None,
    ));
    let (stream, synack) = session.open_stream().await.unwrap();
    let recv = tokio::spawn({
        let session = session.clone();
        async move { session.recv_loop().await }
    });
    let mut wire = BytesMut::new();
    FrameCodec
        .encode(
            Frame::data(stream.id(), Bytes::from_static(b"tail")),
            &mut wire,
        )
        .unwrap();
    FrameCodec
        .encode(Frame::control(Command::Fin, stream.id()), &mut wire)
        .unwrap();
    peer.write_all(&wire).await.unwrap();
    assert!(timeout(DEADLINE, synack).await.unwrap().unwrap().is_err());
    assert!(stream.is_closed());
    assert!(!session.has_active_streams().await);
    assert!(stream.send_data(Bytes::from_static(b"late")).await.is_err());
    let mut reader = stream.reader().lock().await;
    let mut data = [0; 8];
    assert_eq!(reader.read(&mut data).await.unwrap(), 4);
    assert_eq!(&data[..4], b"tail");
    assert_eq!(
        timeout(DEADLINE, reader.read(&mut data))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    session.close().await.unwrap();
    timeout(DEADLINE, recv).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn local_error_emits_fin_and_evicts_stream() {
    let (writer, mut peer) = duplex(128);
    let padding = Arc::new(PaddingFactory::new(b"stop=0").unwrap());
    let session = Arc::new(Session::new_client(empty(), writer, padding, None));
    let (stream, synack) = session.open_stream().await.unwrap();
    let process = tokio::spawn({
        let session = session.clone();
        async move { session.process_stream_data().await }
    });
    stream
        .close_with_error(anytls_rs::util::AnyTlsError::Protocol("test".into()))
        .await;
    assert!(synack.await.unwrap().is_err());
    let mut pending = BytesMut::new();
    let mut codec = FrameCodec;
    let mut frames = Vec::new();
    timeout(DEADLINE, async {
        loop {
            while let Some(frame) = codec.decode(&mut pending).unwrap() {
                let is_fin = frame.cmd == Command::Fin;
                frames.push(frame);
                if is_fin {
                    return;
                }
            }

            let mut chunk = [0; 128];
            let count = peer.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "peer closed before receiving FIN");
            pending.extend_from_slice(&chunk[..count]);
        }
    })
    .await
    .unwrap();
    assert_eq!(frames.last().unwrap().cmd, Command::Fin);
    assert!(!session.has_active_streams().await);

    session.close().await.unwrap();
    let mut tail = Vec::new();
    timeout(DEADLINE, peer.read_to_end(&mut tail))
        .await
        .unwrap()
        .unwrap();
    pending.extend_from_slice(&tail);
    while let Some(frame) = codec.decode(&mut pending).unwrap() {
        frames.push(frame);
    }
    assert!(pending.is_empty());
    assert_eq!(
        frames.iter().map(|frame| frame.cmd).collect::<Vec<_>>(),
        [Command::Settings, Command::Syn, Command::Fin]
    );
    assert_eq!(frames[1].stream_id, stream.id());
    assert_eq!(frames[2].stream_id, stream.id());
    timeout(DEADLINE, process).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn alert_closes_all_stream_maps_and_processor() {
    let (reader, mut peer) = duplex(128);
    let session = Arc::new(Session::new_client(
        reader,
        sink(),
        PaddingFactory::default(),
        None,
    ));
    let (stream, synack) = session.open_stream().await.unwrap();
    let recv = tokio::spawn({
        let session = session.clone();
        async move { session.recv_loop().await }
    });
    let process = tokio::spawn({
        let session = session.clone();
        async move { session.process_stream_data().await }
    });
    let mut wire = BytesMut::new();
    FrameCodec
        .encode(
            Frame::with_data(Command::Alert, 0, Bytes::from_static(b"test")),
            &mut wire,
        )
        .unwrap();
    peer.write_all(&wire).await.unwrap();
    assert!(timeout(DEADLINE, recv).await.unwrap().unwrap().is_err());
    timeout(DEADLINE, process).await.unwrap().unwrap().unwrap();
    assert!(stream.is_closed());
    assert!(synack.await.unwrap().is_err());
    assert!(!session.has_active_streams().await);
}
