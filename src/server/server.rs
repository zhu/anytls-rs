//! AnyTLS Server implementation

use crate::padding::PaddingFactory;
use crate::server::handler::{StreamHandler, TcpProxyHandler};
use crate::session::Session;
use crate::util::{
    AnyTlsError, Result, StringMap, authenticate_client, configure_tcp_stream, hash_password,
};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tracing::{Instrument, field, info_span};

const DEFAULT_MAX_CONNECTIONS: usize = 1024;
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

type StreamCallback = Arc<dyn Fn(Arc<crate::session::Stream>) + Send + Sync + 'static>;

/// Server manages AnyTLS server connections
pub struct Server {
    password_hash: [u8; 32],
    tls_config: Arc<RwLock<Arc<TlsAcceptor>>>,
    padding: Arc<PaddingFactory>,
    on_new_stream: Option<StreamCallback>,
    server_settings: Option<StringMap>,
    connection_limit: Arc<Semaphore>,
    handshake_timeout: Duration,
}

impl Server {
    /// Create a new server
    pub fn new(
        password: &str,
        tls_config: Arc<TlsAcceptor>,
        padding: Arc<PaddingFactory>,
        server_settings: Option<StringMap>,
    ) -> Self {
        let password_hash = hash_password(password);

        Self {
            password_hash,
            tls_config: Arc::new(RwLock::new(tls_config)),
            padding,
            on_new_stream: None,
            server_settings,
            connection_limit: Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }

    /// Create a new server with reloadable TLS config
    pub fn new_with_reloadable_tls(
        password: &str,
        tls_config: Arc<RwLock<Arc<TlsAcceptor>>>,
        padding: Arc<PaddingFactory>,
        server_settings: Option<StringMap>,
    ) -> Self {
        let password_hash = hash_password(password);

        Self {
            password_hash,
            tls_config,
            padding,
            on_new_stream: None,
            server_settings,
            connection_limit: Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }

    /// Set callback for new streams
    pub fn with_stream_handler<F>(mut self, callback: F) -> Self
    where
        F: Fn(Arc<crate::session::Stream>) + Send + Sync + 'static,
    {
        self.on_new_stream = Some(Arc::new(callback));
        self
    }

    /// Configure the maximum live connections and the combined TLS/auth deadline.
    pub fn with_connection_limits(
        mut self,
        max_connections: usize,
        handshake_timeout: Duration,
    ) -> Result<Self> {
        if max_connections == 0 || max_connections > Semaphore::MAX_PERMITS {
            return Err(AnyTlsError::Config(
                "max_connections is out of range".into(),
            ));
        }
        if handshake_timeout.is_zero() {
            return Err(AnyTlsError::Config(
                "handshake_timeout must be positive".into(),
            ));
        }
        self.connection_limit = Arc::new(Semaphore::new(max_connections));
        self.handshake_timeout = handshake_timeout;
        Ok(self)
    }

    /// Start the server and listen for connections.
    pub async fn listen(&self, addr: &str) -> Result<()> {
        self.serve(TcpListener::bind(addr).await?).await
    }

    /// Serve an existing listener. Dropping this future cancels its connections.
    pub async fn serve(&self, listener: TcpListener) -> Result<()> {
        tracing::info!("[Server] Listening on {}", listener.local_addr()?);
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                result = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = result {
                        tracing::error!("[Server] Connection task failed: {}", error);
                    }
                }
                accepted = listener.accept() => {
                    let (stream, addr) = match accepted {
                        Ok(connection) => connection,
                        Err(error) => {
                            // A reset queued connection or temporary descriptor pressure
                            // must not terminate the listener and all healthy sessions.
                            tracing::warn!("[Server] Accept failed: {}", error);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let Ok(permit) = self.connection_limit.clone().try_acquire_owned() else {
                        tracing::debug!("[Server] Connection limit reached; rejecting {}", addr);
                        continue;
                    };
                    let tls_config = self.tls_config.read().unwrap().clone();
                    let password_hash = self.password_hash;
                    let padding = Arc::clone(&self.padding);
                    let on_new_stream = self.on_new_stream.clone();
                    let server_settings = self.server_settings.clone();
                    let handshake_timeout = self.handshake_timeout;
                    let span = info_span!(
                        "anytls.connection",
                        peer_addr = %addr,
                        session_id = field::Empty
                    );
                    connections.spawn(async move {
                        // The permit covers TLS, authentication, and the complete session.
                        let _permit = permit;
                        if let Err(error) = handle_connection(
                            stream, tls_config, password_hash, padding, on_new_stream,
                            server_settings, handshake_timeout,
                        ).await {
                            tracing::debug!("[Server] Connection ended: {}", error);
                        }
                    }.instrument(span));
                }
            }
        }
    }
}

/// Handle a single TCP connection
async fn handle_connection(
    tcp_stream: tokio::net::TcpStream,
    tls_config: Arc<TlsAcceptor>,
    password_hash: [u8; 32],
    padding: Arc<PaddingFactory>,
    on_new_stream: Option<StreamCallback>,
    server_settings: Option<StringMap>,
    handshake_timeout: Duration,
) -> Result<()> {
    let peer_addr = tcp_stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    configure_tcp_stream(&tcp_stream, &peer_addr);
    let handshake_span = info_span!(
        "anytls.handshake",
        peer_addr = %peer_addr,
        session_id = field::Empty,
        tls_version = field::Empty,
        cipher_suite = field::Empty
    );
    tracing::info!("[Server] New connection from {}", peer_addr);
    let (reader, writer) = tokio::time::timeout(
        handshake_timeout,
        async {
            let tls_stream = tls_config
                .accept(tcp_stream)
                .await
                .map_err(|error| AnyTlsError::Tls(format!("TLS handshake failed: {}", error)))?;
            let (_, connection) = tls_stream.get_ref();
            if let Some(protocol) = connection.protocol_version() {
                handshake_span.record("tls_version", field::debug(protocol));
            }
            if let Some(suite) = connection.negotiated_cipher_suite() {
                handshake_span.record("cipher_suite", field::debug(suite.suite()));
            }
            let (mut reader, writer) = tokio::io::split(tls_stream);
            authenticate_client(&mut reader, &password_hash, &padding).await?;
            Ok::<_, AnyTlsError>((reader, writer))
        }
        .instrument(handshake_span.clone()),
    )
    .await
    .map_err(|_| {
        AnyTlsError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "TLS/authentication deadline exceeded",
        ))
    })??;

    // Create callback channel for new streams
    let (stream_callback_tx, stream_callback_rx) =
        tokio::sync::mpsc::unbounded_channel::<Arc<crate::session::Stream>>();

    // Create server session
    let mut session = Session::new_server(reader, writer, padding);
    session.set_server_settings(server_settings.clone());

    // Set callback channel in session
    session.set_stream_callback(stream_callback_tx);

    let session = Arc::new(session);
    let session_id = session.id();
    tracing::Span::current().record("session_id", session_id);
    handshake_span.record("session_id", field::display(session_id));

    tracing::info!(
        session_id = session_id,
        peer_addr = %peer_addr,
        "[Server] Session {} created",
        session_id
    );

    run_session(session, stream_callback_rx, on_new_stream).await
}

/// Own every task for this connection so a closed peer cannot retain its session.
async fn run_session(
    session: Arc<Session>,
    stream_callback_rx: mpsc::UnboundedReceiver<Arc<crate::session::Stream>>,
    callback: Option<StreamCallback>,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let receiver = session.clone();
    tasks.spawn(async move { receiver.recv_loop().await });
    let writer = session.clone();
    tasks.spawn(async move { writer.process_stream_data().await });
    tasks.spawn(run_stream_handlers(
        session.clone(),
        stream_callback_rx,
        callback,
    ));

    let result = tokio::select! {
        _ = session.closed() => Ok(()),
        result = tasks.join_next() => match result {
            Some(Ok(result)) => result,
            Some(Err(error)) => Err(AnyTlsError::Io(std::io::Error::other(error))),
            None => Ok(()),
        }
    };
    // close() cancels blocked reads/writes and wakes every live stream.
    session.close().await?;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    match result {
        Err(AnyTlsError::SessionClosed) => Ok(()),
        other => other,
    }
}

async fn run_stream_handlers(
    session: Arc<Session>,
    mut streams: mpsc::UnboundedReceiver<Arc<crate::session::Stream>>,
    callback: Option<StreamCallback>,
) -> Result<()> {
    let mut handlers = JoinSet::new();
    loop {
        tokio::select! {
            _ = session.closed() => return Ok(()),
            result = handlers.join_next(), if !handlers.is_empty() => {
                if let Some(Err(error)) = result {
                    tracing::error!("[Server] Stream task failed: {}", error);
                }
            }
            stream = streams.recv() => {
                let Some(stream) = stream else { return Ok(()); };
                let session = session.clone();
                let callback = callback.clone();
                let span = info_span!("anytls.stream.proxy", session_id = session.id(), stream_id = stream.id());
                handlers.spawn(async move {
                    if let Some(callback) = callback {
                        callback(stream);
                    } else if let Err(error) = TcpProxyHandler::new().handle_stream(stream, session).await {
                        tracing::debug!("[Proxy] Handler ended: {}", error);
                    }
                }.instrument(span));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{create_server_config, generate_key_pair};
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex, sink};
    use tokio::net::TcpStream;
    use tokio::time::timeout;
    use tokio_rustls::TlsConnector;

    const TEST_DEADLINE: Duration = Duration::from_secs(3);

    fn server() -> Server {
        Server::new(
            "test-password",
            Arc::new(TlsAcceptor::from(create_server_config().unwrap())),
            PaddingFactory::default(),
            None,
        )
    }

    #[tokio::test]
    async fn incomplete_tls_handshake_has_a_deadline() {
        let server = server();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (tcp, _) = listener.accept().await.unwrap();
        let result = timeout(
            TEST_DEADLINE,
            handle_connection(
                tcp,
                server.tls_config.read().unwrap().clone(),
                server.password_hash,
                server.padding,
                None,
                None,
                Duration::from_millis(50),
            ),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Err(AnyTlsError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut)
        );
        assert_eq!(
            timeout(TEST_DEADLINE, client.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn completed_tls_without_authentication_has_a_deadline() {
        let (cert, key) = generate_key_pair().unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (accepted, _) = listener.accept().await.unwrap();
        let handler = tokio::spawn(handle_connection(
            accepted,
            Arc::new(TlsAcceptor::from(Arc::new(server_config))),
            hash_password("test-password"),
            PaddingFactory::default(),
            None,
            None,
            Duration::from_millis(500),
        ));
        let connector = TlsConnector::from(Arc::new(client_config));
        let _tls = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        let result = timeout(TEST_DEADLINE, handler).await.unwrap().unwrap();
        assert!(
            matches!(result, Err(AnyTlsError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut)
        );
    }

    #[tokio::test]
    async fn peer_eof_releases_session_and_callback_cycle() {
        let (reader, mut peer) = duplex(64);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut session = Session::new_server(reader, sink(), PaddingFactory::default());
        session.set_stream_callback(tx);
        let session = Arc::new(session);
        let weak = Arc::downgrade(&session);
        let handler = tokio::spawn(run_session(session, rx, None));
        peer.shutdown().await.unwrap();
        timeout(TEST_DEADLINE, handler)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            weak.upgrade().is_none(),
            "server tasks retained a closed session"
        );
    }

    async fn wait_for_permits(server: &Server, count: usize) {
        timeout(TEST_DEADLINE, async {
            while server.connection_limit.available_permits() != count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connection_limit_rejects_excess_and_recovers_after_timeout() {
        let server = Arc::new(
            server()
                .with_connection_limits(1, Duration::from_millis(500))
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = tokio::spawn({
            let server = server.clone();
            async move { server.serve(listener).await }
        });
        let mut first = TcpStream::connect(address).await.unwrap();
        wait_for_permits(&server, 0).await;
        let mut excess = TcpStream::connect(address).await.unwrap();
        assert_eq!(
            timeout(TEST_DEADLINE, excess.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(
            timeout(TEST_DEADLINE, first.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        wait_for_permits(&server, 1).await;
        let mut next = TcpStream::connect(address).await.unwrap();
        wait_for_permits(&server, 0).await;
        // Dropping the serving future also owns and cancels accepted connections.
        serving.abort();
        let _ = serving.await;
        wait_for_permits(&server, 1).await;
        assert_eq!(
            timeout(TEST_DEADLINE, next.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
