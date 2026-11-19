use anytls_rs::padding::PaddingFactory;
use anytls_rs::session::Session;
use bytes::Bytes;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncWrite, duplex, empty, sink};
use tokio::sync::Notify;
use tokio::time::timeout;

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
