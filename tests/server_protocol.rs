use anytls_rs::padding::PaddingFactory;
use anytls_rs::protocol::{Command, Frame, FrameCodec, MAX_FRAME_PAYLOAD_SIZE};
use anytls_rs::session::{Session, Stream};
use bytes::{Bytes, BytesMut};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex, empty};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::codec::{Decoder, Encoder};

const DEADLINE: Duration = Duration::from_secs(2);

async fn feed_server(frames: Vec<Frame>) -> (bool, Vec<Arc<Stream>>, Vec<Frame>) {
    let (transport, peer) = duplex(65536);
    let (reader, writer) = tokio::io::split(transport);
    let (mut peer_reader, mut peer_writer) = tokio::io::split(peer);
    let mut server = Session::new_server(reader, writer, PaddingFactory::default());
    let (streams_tx, mut streams_rx) = mpsc::unbounded_channel();
    server.set_stream_callback(streams_tx);
    let server = Arc::new(server);
    let recv = tokio::spawn({
        let server = server.clone();
        async move { server.recv_loop().await }
    });
    let mut wire = BytesMut::new();
    for frame in frames {
        FrameCodec.encode(frame, &mut wire).unwrap();
    }
    peer_writer.write_all(&wire).await.unwrap();
    peer_writer.shutdown().await.unwrap();
    let accepted = timeout(DEADLINE, recv).await.unwrap().unwrap().is_ok();
    let mut streams = Vec::new();
    while let Some(stream) = streams_rx.recv().await {
        streams.push(stream);
    }
    let mut response = Vec::new();
    timeout(DEADLINE, peer_reader.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let mut response = BytesMut::from(response.as_slice());
    let mut replies = Vec::new();
    while let Some(frame) = FrameCodec.decode(&mut response).unwrap() {
        replies.push(frame);
    }
    assert!(response.is_empty());
    assert!(!server.has_active_streams().await);
    (accepted, streams, replies)
}

fn settings(version: u8) -> Frame {
    Frame::with_data(Command::Settings, 0, Bytes::from(format!("v={version}")))
}

#[tokio::test]
async fn syn_before_settings_is_rejected_with_alert() {
    let (accepted, streams, replies) = feed_server(vec![Frame::control(Command::Syn, 1)]).await;
    assert!(!accepted);
    assert!(streams.is_empty());
    assert!(replies.iter().any(|frame| frame.cmd == Command::Alert));
}

#[tokio::test]
async fn v1_and_v2_settings_allow_monotonically_increasing_ids_with_gaps() {
    for version in [1, 2] {
        let (accepted, streams, replies) = feed_server(vec![
            settings(version),
            Frame::control(Command::Syn, 1),
            Frame::control(Command::Syn, 3),
        ])
        .await;
        assert!(accepted);
        assert_eq!(
            streams.iter().map(|stream| stream.id()).collect::<Vec<_>>(),
            [1, 3]
        );
        assert_eq!(
            replies
                .iter()
                .any(|frame| frame.cmd == Command::ServerSettings),
            version == 2
        );
    }
}

#[tokio::test]
async fn zero_duplicate_decreasing_and_reused_stream_ids_are_rejected() {
    for sequence in [vec![0], vec![1, 1], vec![3, 2]] {
        let mut frames = vec![settings(2)];
        frames.extend(sequence.iter().map(|id| Frame::control(Command::Syn, *id)));
        let (accepted, streams, replies) = feed_server(frames).await;
        assert!(!accepted, "invalid sequence {sequence:?}");
        assert_eq!(streams.len(), sequence.len() - 1);
        assert!(replies.iter().any(|frame| frame.cmd == Command::Alert));
    }
    let (accepted, streams, _) = feed_server(vec![
        settings(2),
        Frame::control(Command::Syn, 1),
        Frame::control(Command::Fin, 1),
        Frame::control(Command::Syn, 1),
    ])
    .await;
    assert!(!accepted);
    assert_eq!(streams.len(), 1);
}

#[tokio::test]
async fn empty_push_is_not_stream_eof() {
    let (accepted, streams, _) = feed_server(vec![
        settings(2),
        Frame::control(Command::Syn, 1),
        Frame::data(1, Bytes::new()),
        Frame::data(1, Bytes::from_static(b"payload")),
        Frame::control(Command::Fin, 1),
    ])
    .await;
    assert!(accepted);
    let mut reader = streams[0].reader().lock().await;
    let mut data = [0; 7];
    timeout(DEADLINE, reader.read_exact(&mut data))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&data, b"payload");
    assert_eq!(reader.read(&mut data).await.unwrap(), 0);
}

#[tokio::test]
async fn large_application_write_is_split_into_valid_frames() {
    let data = Bytes::from(vec![0x5a; MAX_FRAME_PAYLOAD_SIZE * 2 + 1]);
    let (writer, mut peer) = duplex(data.len() + 64);
    let session = Session::new_server(empty(), writer, PaddingFactory::default());
    timeout(DEADLINE, session.write_data_frame(1, data.clone()))
        .await
        .unwrap()
        .unwrap();
    session.close().await.unwrap();
    let mut wire = Vec::new();
    timeout(DEADLINE, peer.read_to_end(&mut wire))
        .await
        .unwrap()
        .unwrap();
    let mut wire = BytesMut::from(wire.as_slice());
    let mut reconstructed = Vec::new();
    let mut sizes = Vec::new();
    while let Some(frame) = FrameCodec.decode(&mut wire).unwrap() {
        assert_eq!(frame.cmd, Command::Push);
        assert_eq!(frame.stream_id, 1);
        sizes.push(frame.data.len());
        reconstructed.extend_from_slice(&frame.data);
    }
    assert!(wire.is_empty());
    assert_eq!(sizes, [MAX_FRAME_PAYLOAD_SIZE, MAX_FRAME_PAYLOAD_SIZE, 1]);
    assert_eq!(reconstructed, data);
}

#[test]
fn oversized_frame_is_rejected_before_modifying_output() {
    let mut wire = BytesMut::from(&b"prefix"[..]);
    let error = FrameCodec
        .encode(
            Frame::with_data(
                Command::Settings,
                0,
                Bytes::from(vec![0; MAX_FRAME_PAYLOAD_SIZE + 1]),
            ),
            &mut wire,
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(&wire[..], b"prefix");
}
