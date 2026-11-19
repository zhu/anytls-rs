use anytls_rs::padding::PaddingFactory;
use anytls_rs::protocol::{Command, Frame, FrameCodec};
use anytls_rs::server::handler::{StreamHandler, TcpProxyHandler};
use anytls_rs::session::Session;
use bytes::{Bytes, BytesMut};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, duplex, empty};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};
use tokio::time::{sleep, timeout};
use tokio_util::codec::{Decoder, Encoder};

const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Gate {
    wire: Mutex<Vec<u8>>,
    blocked: Notify,
    open: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Gate {
    fn release(&self) {
        self.open.store(true, Ordering::Release);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }
}

struct PartialAckWriter {
    gate: Arc<Gate>,
    partial_ack: bool,
}

impl PartialAckWriter {
    fn new(gate: Arc<Gate>) -> Self {
        Self {
            gate,
            partial_ack: false,
        }
    }
}

impl AsyncWrite for PartialAckWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.partial_ack && data.first() == Some(&(Command::SynAck as u8)) {
            self.partial_ack = true;
            self.gate.wire.lock().unwrap().extend_from_slice(&data[..3]);
            return Poll::Ready(Ok(3));
        }
        if self.partial_ack && !self.gate.open.load(Ordering::Acquire) {
            *self.gate.waker.lock().unwrap() = Some(cx.waker().clone());
            self.gate.blocked.notify_one();
            return Poll::Pending;
        }
        self.gate.wire.lock().unwrap().extend_from_slice(data);
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn peer_fin_cannot_interrupt_a_partially_written_synack() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = target.local_addr().unwrap();
    let gate = Arc::new(Gate::default());
    let (reader, mut peer) = duplex(65536);
    let mut session = Session::new_server(
        reader,
        PartialAckWriter::new(gate.clone()),
        PaddingFactory::default(),
    );
    let (streams_tx, mut streams_rx) = mpsc::unbounded_channel();
    session.set_stream_callback(streams_tx);
    let session = Arc::new(session);
    let recv = tokio::spawn({
        let session = session.clone();
        async move { session.recv_loop().await }
    });
    let process = tokio::spawn({
        let session = session.clone();
        async move { session.process_stream_data().await }
    });
    let mut request = vec![1, 127, 0, 0, 1];
    request.extend_from_slice(&address.port().to_be_bytes());
    let mut wire = BytesMut::new();
    for frame in [
        Frame::with_data(Command::Settings, 0, Bytes::from_static(b"v=2")),
        Frame::control(Command::Syn, 1),
        Frame::data(1, Bytes::from(request)),
    ] {
        FrameCodec.encode(frame, &mut wire).unwrap();
    }
    peer.write_all(&wire).await.unwrap();
    let stream = timeout(DEADLINE, streams_rx.recv()).await.unwrap().unwrap();
    let handler = tokio::spawn({
        let session = session.clone();
        let stream = stream.clone();
        async move { TcpProxyHandler::new().handle_stream(stream, session).await }
    });
    let (mut target_connection, _) = timeout(DEADLINE, target.accept()).await.unwrap().unwrap();
    timeout(DEADLINE, gate.blocked.notified()).await.unwrap();
    wire.clear();
    FrameCodec
        .encode(Frame::control(Command::Fin, 1), &mut wire)
        .unwrap();
    peer.write_all(&wire).await.unwrap();
    timeout(DEADLINE, stream.closed()).await.unwrap();

    // Go beyond the stream's drain grace. The ACK is shared transport state,
    // so only a whole-session failure may interrupt it.
    sleep(Duration::from_millis(1100)).await;
    assert!(!handler.is_finished(), "FIN cancelled an in-progress ACK");
    assert!(
        !session.is_closed(),
        "ordinary stream FIN closed its session"
    );
    let following = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .write_control_frame(Frame::control(Command::HeartResponse, 0))
                .await
        }
    });
    gate.release();
    timeout(DEADLINE, handler).await.unwrap().unwrap().unwrap();
    timeout(DEADLINE, following)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!session.is_closed());
    assert_eq!(
        timeout(DEADLINE, target_connection.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );

    let mut emitted = BytesMut::from(gate.wire.lock().unwrap().as_slice());
    let mut frames = Vec::new();
    while let Some(frame) = FrameCodec.decode(&mut emitted).unwrap() {
        frames.push(frame);
    }
    assert!(emitted.is_empty());
    assert_eq!(
        frames.iter().map(|f| f.cmd).collect::<Vec<_>>(),
        [
            Command::ServerSettings,
            Command::SynAck,
            Command::HeartResponse
        ]
    );
    assert_eq!(frames[1].stream_id, 1);
    assert!(frames[1].data.is_empty());
    session.close().await.unwrap();
    timeout(DEADLINE, recv).await.unwrap().unwrap().unwrap();
    timeout(DEADLINE, process).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn aborted_partial_frame_closes_session_and_prevents_waiting_writer_reuse() {
    let gate = Arc::new(Gate::default());
    let session = Arc::new(Session::new_server(
        empty(),
        PartialAckWriter::new(gate.clone()),
        PaddingFactory::default(),
    ));
    let first = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .write_control_frame(Frame::control(Command::SynAck, 1))
                .await
        }
    });
    timeout(DEADLINE, gate.blocked.notified()).await.unwrap();
    let waiting = Arc::new(Notify::new());
    let following = tokio::spawn({
        let session = session.clone();
        let waiting = waiting.clone();
        async move {
            waiting.notify_one();
            session
                .write_control_frame(Frame::control(Command::HeartResponse, 0))
                .await
        }
    });
    timeout(DEADLINE, waiting.notified()).await.unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    gate.release();
    assert!(
        timeout(DEADLINE, following)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(session.is_closed());
    assert_eq!(
        gate.wire.lock().unwrap().len(),
        3,
        "another frame followed a truncated ACK"
    );
    assert!(
        session
            .write_control_frame(Frame::control(Command::HeartResponse, 0))
            .await
            .is_err()
    );
    session.close().await.unwrap();
}
