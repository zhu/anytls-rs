//! Stream implementation for AnyTLS protocol
//!
//! Stream provides a duplex communication channel that implements AsyncRead and AsyncWrite

use crate::session::StreamReader;
use crate::util::{AnyTlsError, Result};
use bytes::Bytes;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot, watch};

/// Stream represents a single data stream within a Session
/// It implements AsyncRead and AsyncWrite to be used as a connection
pub struct Stream {
    id: u32,

    // ===== 读取部分：使用独立的 StreamReader =====
    // Arc<Mutex<>> 是为了在 poll_read 中获取 &mut
    reader: Arc<tokio::sync::Mutex<StreamReader>>,

    // ===== 写入部分：直接使用 channel，无需锁 =====
    writer_tx: mpsc::Sender<(u32, Bytes)>,

    // ===== SYNACK 通知 (用于超时检测) =====
    synack_tx: std::sync::Mutex<Option<oneshot::Sender<Result<()>>>>,

    // ===== 状态管理 =====
    is_closed: Arc<AtomicBool>,
    close_notify: watch::Sender<bool>,

    // Guards the one-shot stream-close sentinel so a FIN is emitted at most
    // once regardless of how many times `close()`/`poll_shutdown` fire.
    fin_sent: Arc<AtomicBool>,
}

impl Stream {
    /// Create a new stream
    ///
    /// # Arguments
    /// * `id` - Stream ID
    /// * `reader` - StreamReader 用于读取数据
    /// * `writer_tx` - 发送数据到 Session 的 channel
    ///
    /// # Returns
    /// (Stream, Receiver) - The receiver can be used to wait for SYNACK
    pub fn new(
        id: u32,
        mut reader: StreamReader,
        writer_tx: mpsc::Sender<(u32, Bytes)>,
    ) -> (Self, oneshot::Receiver<Result<()>>) {
        let (synack_tx, synack_rx) = oneshot::channel();
        let (close_notify, close_rx) = watch::channel(false);
        reader.set_close_notification(close_rx);

        let stream = Self {
            id,
            reader: Arc::new(tokio::sync::Mutex::new(reader)),
            writer_tx,
            synack_tx: std::sync::Mutex::new(Some(synack_tx)),
            is_closed: Arc::new(AtomicBool::new(false)),
            close_notify,
            fin_sent: Arc::new(AtomicBool::new(false)),
        };

        (stream, synack_rx)
    }

    /// Notify that SYNACK has been received
    ///
    /// # Arguments
    /// * `result` - Ok(()) for success, Err for error
    pub async fn notify_synack(&self, result: Result<()>) {
        self.finish_synack(result);
    }

    fn finish_synack(&self, result: Result<()>) {
        if let Some(tx) = self.synack_tx.lock().expect("SYNACK mutex poisoned").take() {
            let _ = tx.send(result);
        }
    }

    fn mark_closed(&self, error: AnyTlsError) {
        self.is_closed.store(true, Ordering::Release);
        self.close_notify.send_replace(true);
        self.finish_synack(Err(error));
    }

    /// Close after a peer FIN or session failure, without echoing a FIN.
    pub(crate) fn close_locally(&self, error: AnyTlsError) {
        self.fin_sent.store(true, Ordering::Release);
        self.mark_closed(error);
    }

    /// Wait for local, remote, or session closure, without losing early closes.
    pub async fn closed(&self) {
        let mut closed = self.close_notify.subscribe();
        let _ = closed.wait_for(|closed| *closed).await;
    }

    /// Get stream ID
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Close the stream with error (can be called with `Arc<Stream>`)
    pub async fn close_with_error(&self, err: AnyTlsError) {
        self.mark_closed(err);
        self.close();
    }

    /// Check if stream is closed
    pub fn is_closed(&self) -> bool {
        self.is_closed.load(Ordering::Relaxed)
    }

    /// Close this stream and enqueue its FIN after all previously queued data.
    ///
    /// Signals the owning session to emit a `Fin` frame for this stream id and
    /// evict it from the session's stream maps. An empty `Bytes` value marks
    /// the FIN in the internal writer channel; public empty data writes are
    /// ignored before they reach that channel.
    ///
    /// Synchronous and idempotent, so it is safe to call from `poll_shutdown`
    /// or a `Drop` impl on a wrapper type.
    pub fn close(&self) {
        self.mark_closed(AnyTlsError::Io(std::io::ErrorKind::BrokenPipe.into()));
        if self
            .fin_sent
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let fin = (self.id, Bytes::new());
        match self.writer_tx.try_send(fin) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(fin)) => {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let writer_tx = self.writer_tx.clone();
                    handle.spawn(async move {
                        let _ = writer_tx.send(fin).await;
                    });
                } else {
                    self.fin_sent.store(false, Ordering::Release);
                }
            }
        }
    }

    /// Get a reference to the reader (for direct access in handlers)
    pub fn reader(&self) -> &Arc<tokio::sync::Mutex<StreamReader>> {
        &self.reader
    }

    /// Send data through the writer channel (无锁方式)
    ///
    /// 这个方法可以被多个任务并发调用，无需任何锁
    pub async fn send_data(
        &self,
        data: Bytes,
    ) -> std::result::Result<(), mpsc::error::SendError<(u32, Bytes)>> {
        if self.is_closed() {
            return Err(mpsc::error::SendError((self.id, data)));
        }
        // Empty data is not a close request. Only close()/shutdown() enqueue
        // the internal FIN sentinel.
        if data.is_empty() {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = self.closed() => Err(mpsc::error::SendError((self.id, data.clone()))),
            result = self.writer_tx.send((self.id, data.clone())) => result,
        }
    }
}

// Stream is not meant to be cloned - use Arc<Stream> instead
// This implementation is only for compatibility with HashMap storage

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let stream_id = self.id;
        let remaining = buf.remaining();

        // 使用 tokio::task::block_in_place 同步读取
        // 这避免了复杂的 Future polling 和借用问题
        let reader = Arc::clone(&self.reader);

        // 创建读取 future
        let mut read_fut = std::pin::pin!(async move {
            let mut reader_guard = reader.lock().await;
            let mut temp_buf = vec![0u8; remaining];
            let n = reader_guard.read(&mut temp_buf).await?;
            Ok::<(usize, Vec<u8>), std::io::Error>((n, temp_buf))
        });

        // Poll the future
        match read_fut.as_mut().poll(cx) {
            Poll::Ready(Ok((n, temp_buf))) => {
                if n > 0 {
                    buf.put_slice(&temp_buf[..n]);
                    tracing::trace!(
                        "[Stream] poll_read: Read {} bytes (stream_id={})",
                        n,
                        stream_id
                    );
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                tracing::error!(
                    "[Stream] poll_read: Error reading (stream_id={}): {}",
                    stream_id,
                    e
                );
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let stream_id = self.id;
        let buf_len = buf.len();
        tracing::trace!(
            "[Stream] poll_write: stream_id={}, buf_len={}",
            stream_id,
            buf_len
        );

        if self.is_closed.load(Ordering::Relaxed) {
            tracing::warn!("[Stream] poll_write: Stream {} is closed", stream_id);
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stream closed",
            )));
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // Send data to session via channel
        let data = Bytes::copy_from_slice(buf);
        tracing::trace!(
            "[Stream] poll_write: Sending {} bytes to channel for stream {}",
            buf_len,
            stream_id
        );
        match self.writer_tx.try_send((self.id, data)) {
            Ok(_) => {
                tracing::debug!(
                    "[Stream] poll_write: Successfully sent {} bytes to channel for stream {}",
                    buf_len,
                    stream_id
                );
                Poll::Ready(Ok(buf.len()))
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let writer_tx = self.writer_tx.clone();
                    let waker = cx.waker().clone();
                    let mut closed = self.close_notify.subscribe();
                    handle.spawn(async move {
                        tokio::select! {
                            _ = writer_tx.reserve() => {},
                            _ = closed.wait_for(|closed| *closed) => {},
                        }
                        waker.wake();
                    });
                } else {
                    cx.waker().wake_by_ref();
                }
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(e)) => {
                tracing::error!(
                    "[Stream] poll_write: Failed to send {} bytes to channel for stream {}: {:?}",
                    buf_len,
                    stream_id,
                    e
                );
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "session channel closed",
                )))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.mark_closed(AnyTlsError::Io(std::io::ErrorKind::BrokenPipe.into()));
        if self.fin_sent.load(Ordering::Acquire) {
            return Poll::Ready(Ok(()));
        }

        match self.writer_tx.try_send((self.id, Bytes::new())) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.fin_sent.store(true, Ordering::Release);
                Poll::Ready(Ok(()))
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let writer_tx = self.writer_tx.clone();
                    let waker = cx.waker().clone();
                    handle.spawn(async move {
                        let _ = writer_tx.reserve().await;
                        waker.wake();
                    });
                } else {
                    cx.waker().wake_by_ref();
                }
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::StreamReader;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn remote_close_wakes_read_and_synack_without_echo_fin() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_reader_tx, reader_rx) = mpsc::channel(1);
        let (stream, synack) = Stream::new(1, StreamReader::new(1, reader_rx), tx);
        let stream = Arc::new(stream);
        let read = tokio::spawn({
            let stream = stream.clone();
            async move { stream.reader().lock().await.read(&mut [0; 1]).await }
        });
        tokio::task::yield_now().await;
        stream.close_locally(AnyTlsError::SessionClosed);
        stream.close();
        assert!(stream.is_closed());
        assert!(synack.await.unwrap().is_err());
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), read)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            0
        );
        assert!(rx.try_recv().is_err(), "a received FIN must not be echoed");
    }

    #[tokio::test]
    async fn closing_stream_cancels_backpressured_send() {
        let (tx, _rx) = mpsc::channel(1);
        let (_reader_tx, reader_rx) = mpsc::channel(1);
        let (stream, _) = Stream::new(1, StreamReader::new(1, reader_rx), tx);
        stream
            .send_data(Bytes::from_static(b"first"))
            .await
            .unwrap();
        let send = stream.send_data(Bytes::from_static(b"second"));
        tokio::pin!(send);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut send)
                .await
                .is_err()
        );
        stream.close_locally(AnyTlsError::SessionClosed);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), send)
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn empty_writes_do_not_enqueue_fin() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_reader_tx, reader_rx) = mpsc::channel(1);
        let (mut stream, _) = Stream::new(1, StreamReader::new(1, reader_rx), tx);
        stream.send_data(Bytes::new()).await.unwrap();
        assert_eq!(stream.write(&[]).await.unwrap(), 0);
        assert!(rx.try_recv().is_err());
        assert!(!stream.is_closed());
    }

    #[tokio::test]
    async fn test_stream_write() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_reader_tx, reader_rx) = mpsc::channel(1);

        let reader = StreamReader::new(1, reader_rx);
        let (mut stream, _synack_rx) = Stream::new(1, reader, tx);

        // 写入数据
        stream.write_all(b"hello").await.unwrap();

        // 验证数据发送到 channel
        assert!(matches!(
            rx.recv().await,
            Some((1, data)) if data.as_ref() == b"hello"
        ));
    }

    #[tokio::test]
    async fn test_send_data_waits_when_session_channel_is_full() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_reader_tx, reader_rx) = mpsc::channel(1);

        let reader = StreamReader::new(1, reader_rx);
        let (stream, _synack_rx) = Stream::new(1, reader, tx);

        stream
            .send_data(Bytes::from_static(b"first"))
            .await
            .unwrap();

        let second_send = stream.send_data(Bytes::from_static(b"second"));
        tokio::pin!(second_send);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut second_send)
                .await
                .is_err(),
            "send_data should wait instead of queueing unbounded data"
        );

        assert!(matches!(
            rx.recv().await,
            Some((1, data)) if data.as_ref() == b"first"
        ));

        second_send.await.unwrap();
        assert!(matches!(
            rx.recv().await,
            Some((1, data)) if data.as_ref() == b"second"
        ));
    }

    #[tokio::test]
    async fn test_shutdown_waits_until_fin_can_be_queued() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_reader_tx, reader_rx) = mpsc::channel(1);

        let reader = StreamReader::new(1, reader_rx);
        let (mut stream, _synack_rx) = Stream::new(1, reader, tx);

        stream
            .send_data(Bytes::from_static(b"first"))
            .await
            .unwrap();

        let shutdown = stream.shutdown();
        tokio::pin!(shutdown);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut shutdown)
                .await
                .is_err(),
            "shutdown should wait until the FIN sentinel can be queued"
        );

        assert!(matches!(
            rx.recv().await,
            Some((1, data)) if data.as_ref() == b"first"
        ));

        shutdown.await.unwrap();
        assert!(matches!(rx.recv().await, Some((1, data)) if data.is_empty()));
    }

    #[tokio::test]
    async fn test_stream_read() {
        let (tx, _rx) = mpsc::channel(1);
        let (reader_tx, reader_rx) = mpsc::channel(1);

        let reader = StreamReader::new(1, reader_rx);
        let (mut stream, _synack_rx) = Stream::new(1, reader, tx);

        // 发送数据到 reader
        reader_tx.send(Bytes::from("world")).await.unwrap();

        // 读取数据
        let mut buf = vec![0u8; 10];
        let n = stream.read(&mut buf).await.unwrap();

        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b"world");
    }

    #[tokio::test]
    async fn test_stream_read_write() {
        let (tx, mut rx) = mpsc::channel(1);
        let (reader_tx, reader_rx) = mpsc::channel(1);

        let reader = StreamReader::new(1, reader_rx);
        let (mut stream, _synack_rx) = Stream::new(1, reader, tx);

        // 同时读写
        reader_tx.send(Bytes::from("input")).await.unwrap();
        stream.write_all(b"output").await.unwrap();

        // 验证读取
        let mut buf = vec![0u8; 10];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b"input");

        // 验证写入
        assert!(matches!(
            rx.recv().await,
            Some((1, data)) if data.as_ref() == b"output"
        ));
    }

    #[tokio::test]
    async fn async_write_applies_backpressure_until_session_queue_has_room() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_reader_tx, reader_rx) = mpsc::channel(1);
        let (mut stream, _synack_rx) = Stream::new(1, StreamReader::new(1, reader_rx), tx);

        stream.write_all(b"first").await.unwrap();
        let blocked = tokio::spawn(async move {
            stream.write_all(b"second").await.unwrap();
        });
        tokio::task::yield_now().await;
        assert!(
            !blocked.is_finished(),
            "write should wait for queue capacity"
        );

        assert!(matches!(
            rx.recv().await,
            Some((1, data)) if data.as_ref() == b"first"
        ));
        blocked.await.unwrap();
        assert!(matches!(
            rx.recv().await,
            Some((1, data)) if data.as_ref() == b"second"
        ));
    }
}
