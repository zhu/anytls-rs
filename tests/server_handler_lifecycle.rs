use anytls_rs::padding::PaddingFactory;
use anytls_rs::protocol::{Command, Frame, FrameCodec};
use anytls_rs::server::handler::{StreamHandler, TcpProxyHandler};
use anytls_rs::session::{Session, Stream};
use anytls_rs::util::Result;
use bytes::{Bytes, BytesMut};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::codec::{Decoder, Encoder};

const DEADLINE: Duration = Duration::from_secs(2);

struct Proxy {
    session: Arc<Session>,
    stream: Arc<Stream>,
    peer: DuplexStream,
    buffered: BytesMut,
    recv: JoinHandle<Result<()>>,
    process: JoinHandle<Result<()>>,
    handler: JoinHandle<Result<()>>,
}

impl Proxy {
    async fn new() -> Self {
        Self::with_initial_request(None).await
    }

    async fn with_initial_request(request_and_fin: Option<&[u8]>) -> Self {
        let (transport, mut peer) = duplex(65536);
        let (reader, writer) = tokio::io::split(transport);
        let mut session = Session::new_server(reader, writer, PaddingFactory::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        session.set_stream_callback(tx);
        let session = Arc::new(session);
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
                Frame::with_data(Command::Settings, 0, Bytes::from_static(b"v=2")),
                &mut wire,
            )
            .unwrap();
        FrameCodec
            .encode(Frame::control(Command::Syn, 1), &mut wire)
            .unwrap();
        if let Some(request) = request_and_fin {
            FrameCodec
                .encode(Frame::data(1, Bytes::copy_from_slice(request)), &mut wire)
                .unwrap();
            FrameCodec
                .encode(Frame::control(Command::Fin, 1), &mut wire)
                .unwrap();
        }
        peer.write_all(&wire).await.unwrap();
        let stream = timeout(DEADLINE, rx.recv()).await.unwrap().unwrap();
        let handler = tokio::spawn({
            let stream = stream.clone();
            let session = session.clone();
            async move { TcpProxyHandler::new().handle_stream(stream, session).await }
        });
        Self {
            session,
            stream,
            peer,
            buffered: BytesMut::new(),
            recv,
            process,
            handler,
        }
    }

    async fn send(&mut self, data: &[u8]) {
        let mut wire = BytesMut::new();
        FrameCodec
            .encode(Frame::data(1, Bytes::copy_from_slice(data)), &mut wire)
            .unwrap();
        timeout(DEADLINE, self.peer.write_all(&wire))
            .await
            .unwrap()
            .unwrap();
    }

    async fn send_and_fin(&mut self, data: &[u8]) {
        let mut wire = BytesMut::new();
        for chunk in data.chunks(u16::MAX as usize) {
            FrameCodec
                .encode(Frame::data(1, Bytes::copy_from_slice(chunk)), &mut wire)
                .unwrap();
        }
        FrameCodec
            .encode(Frame::control(Command::Fin, 1), &mut wire)
            .unwrap();
        timeout(DEADLINE, self.peer.write_all(&wire))
            .await
            .unwrap()
            .unwrap();
    }

    async fn next_frame(&mut self) -> Frame {
        timeout(DEADLINE, async {
            loop {
                if let Some(frame) = FrameCodec.decode(&mut self.buffered).unwrap() {
                    return frame;
                }
                assert_ne!(
                    self.peer.read_buf(&mut self.buffered).await.unwrap(),
                    0,
                    "transport closed before FIN"
                );
            }
        })
        .await
        .unwrap()
    }

    async fn until_fin(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        loop {
            let frame = self.next_frame().await;
            let done = frame.cmd == Command::Fin;
            frames.push(frame);
            if done {
                return frames;
            }
        }
    }

    async fn finish(self) -> std::result::Result<Result<()>, tokio::task::JoinError> {
        let handler = timeout(DEADLINE, self.handler).await.unwrap();
        assert!(self.stream.is_closed());
        assert!(!self.session.has_active_streams().await);
        self.session.close().await.unwrap();
        timeout(DEADLINE, self.recv)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(DEADLINE, self.process)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        handler
    }
}

fn ipv4_address(address: SocketAddr) -> Vec<u8> {
    let SocketAddr::V4(address) = address else {
        panic!("test requires IPv4")
    };
    let mut bytes = vec![1];
    bytes.extend_from_slice(&address.ip().octets());
    bytes.extend_from_slice(&address.port().to_be_bytes());
    bytes
}

fn domain_address(domain: &str) -> Vec<u8> {
    let mut bytes = vec![3, domain.len() as u8];
    bytes.extend_from_slice(domain.as_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

#[tokio::test]
async fn fragmented_address_and_target_eof_preserve_tail_before_fin() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = target.local_addr().unwrap();
    let target_task = tokio::spawn(async move {
        let (mut connection, _) = target.accept().await.unwrap();
        connection.write_all(b"tail").await.unwrap();
        connection.shutdown().await.unwrap();
    });
    let mut proxy = Proxy::new().await;
    for byte in ipv4_address(address) {
        proxy.send(&[byte]).await;
    }
    let frames = proxy.until_fin().await;
    let payload: Vec<u8> = frames
        .iter()
        .filter(|f| f.cmd == Command::Push)
        .flat_map(|f| f.data.iter().copied())
        .collect();
    assert_eq!(&payload, b"tail");
    assert!(
        frames
            .iter()
            .any(|f| f.cmd == Command::SynAck && f.data.is_empty())
    );
    assert_eq!(frames.last().unwrap().cmd, Command::Fin);
    proxy.finish().await.unwrap().unwrap();
    timeout(DEADLINE, target_task).await.unwrap().unwrap();
}

#[tokio::test]
async fn initial_request_and_peer_fin_in_one_write_deliver_all_payload() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut request = ipv4_address(target.local_addr().unwrap());
    let payload = vec![0x53; 4096];
    request.extend_from_slice(&payload);
    let proxy = Proxy::with_initial_request(Some(&request)).await;
    let (mut connection, _) = timeout(DEADLINE, target.accept()).await.unwrap().unwrap();
    let mut received = Vec::new();
    timeout(DEADLINE, connection.read_to_end(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received, payload);
    proxy.finish().await.unwrap().unwrap();
}

#[tokio::test]
async fn established_connection_drains_payload_before_peer_fin() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut proxy = Proxy::new().await;
    proxy
        .send(&ipv4_address(target.local_addr().unwrap()))
        .await;
    let (mut connection, _) = timeout(DEADLINE, target.accept()).await.unwrap().unwrap();
    while proxy.next_frame().await.cmd != Command::SynAck {}
    let payload = vec![0x7a; 32768];
    proxy.send_and_fin(&payload).await;
    let mut received = Vec::new();
    timeout(DEADLINE, connection.read_to_end(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.len(), payload.len());
    assert_eq!(received, payload);
    proxy.finish().await.unwrap().unwrap();
}

#[tokio::test]
async fn peer_fin_drain_is_bounded_when_target_stops_reading() {
    let target = TcpSocket::new_v4().unwrap();
    target.set_recv_buffer_size(1024).unwrap();
    target.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let target = target.listen(1).unwrap();
    let mut proxy = Proxy::new().await;
    proxy
        .send(&ipv4_address(target.local_addr().unwrap()))
        .await;
    let (_unread_connection, _) = timeout(DEADLINE, target.accept()).await.unwrap().unwrap();
    while proxy.next_frame().await.cmd != Command::SynAck {}

    // 65 PSH frames fit the receive queue once the relay takes its first
    // frame, so FIN is parsed even though the destination window is blocked.
    let payload = vec![0x35; 4 * 1024 * 1024];
    let started = std::time::Instant::now();
    proxy.send_and_fin(&payload).await;
    timeout(DEADLINE, proxy.stream.closed()).await.unwrap();
    proxy.finish().await.unwrap().unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "test did not exercise a blocked drain"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "closed stream kept the target task alive"
    );
}

#[tokio::test]
async fn refused_target_reports_synack_error_then_fin() {
    let target = TcpSocket::new_v4().unwrap();
    target.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let mut proxy = Proxy::new().await;
    proxy
        .send(&ipv4_address(target.local_addr().unwrap()))
        .await;
    let frames = proxy.until_fin().await;
    assert!(
        frames
            .iter()
            .any(|f| f.cmd == Command::SynAck && !f.data.is_empty())
    );
    assert_eq!(frames.last().unwrap().cmd, Command::Fin);
    assert!(proxy.finish().await.unwrap().is_err());
}

#[tokio::test]
async fn malformed_socks_address_reports_error_and_releases_stream() {
    let mut proxy = Proxy::new().await;
    proxy.send(&[0x7f]).await;
    let frames = proxy.until_fin().await;
    assert!(
        frames
            .iter()
            .any(|f| f.cmd == Command::SynAck && !f.data.is_empty())
    );
    assert!(proxy.finish().await.unwrap().is_err());
}

#[tokio::test]
async fn cancelled_handler_closes_tcp_halves_and_sends_fin() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut proxy = Proxy::new().await;
    proxy
        .send(&ipv4_address(target.local_addr().unwrap()))
        .await;
    let (mut connection, _) = timeout(DEADLINE, target.accept()).await.unwrap().unwrap();
    loop {
        if proxy.next_frame().await.cmd == Command::SynAck {
            break;
        }
    }
    proxy.handler.abort();
    proxy.until_fin().await;
    assert_eq!(
        timeout(DEADLINE, connection.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(proxy.finish().await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn malformed_uot_v1_and_v2_bodies_still_close_their_streams() {
    for (host, invalid_mode) in [
        ("sp.udp-over-tcp.arpa", 0xff),
        ("sp.v2.udp-over-tcp.arpa", 2),
    ] {
        let mut proxy = Proxy::new().await;
        let mut request = domain_address(host);
        request.push(invalid_mode);
        proxy.send(&request).await;
        let frames = proxy.until_fin().await;
        assert!(
            frames
                .iter()
                .any(|f| f.cmd == Command::SynAck && f.data.is_empty())
        );
        assert!(proxy.finish().await.unwrap().is_err());
    }
}
