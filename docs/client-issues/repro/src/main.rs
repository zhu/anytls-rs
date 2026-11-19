#![allow(dead_code)]

pub use anytls_rs::{client, session, util};
mod http_proxy_probe;
mod socks5_probe;

use anyhow::{Result, ensure};
use anytls_rs::{Client, PaddingFactory, Session, SessionPool};
use bytes::Bytes;
use std::{io, pin::Pin, sync::{Arc, Mutex}, task::{Context, Poll}, time::Duration};
use tokio::{io::{AsyncReadExt, AsyncWrite, AsyncWriteExt}, net::{TcpListener, TcpStream}, task::JoinHandle, time::{sleep, timeout}};
use tokio_rustls::{TlsAcceptor, TlsConnector, rustls::pki_types::ServerName};

const BOUND: Duration = Duration::from_secs(3);

fn connector() -> Arc<TlsConnector> {
    Arc::new(TlsConnector::from(util::create_client_config().unwrap()))
}

async fn fixture() -> Result<(Arc<Client>, JoinHandle<anytls_rs::Result<()>>)> {
    fixture_with_config(anytls_rs::SessionPoolConfig::default()).await
}

async fn fixture_with_config(pool_config: anytls_rs::SessionPoolConfig) -> Result<(Arc<Client>, JoinHandle<anytls_rs::Result<()>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let padding = Arc::new(PaddingFactory::new(b"stop=0") .unwrap());
    let server = anytls_rs::server::Server::new("audit-only", Arc::new(TlsAcceptor::from(util::create_server_config()?)), padding.clone(), None);
    let server_task = tokio::spawn(async move { server.serve(listener).await });
    let client = Arc::new(Client::with_pool_config("audit-only", addr.to_string(), ServerName::try_from("localhost")?, connector(), padding, pool_config));
    Ok((client, server_task))
}

async fn pair() -> Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local = TcpStream::connect(listener.local_addr()?).await?;
    let (proxy, _) = listener.accept().await?;
    Ok((local, proxy))
}

async fn read_http_response(socket: &mut TcpStream) -> Result<Vec<u8>> {
    timeout(BOUND, async {
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            response.push(socket.read_u8().await?);
            ensure!(response.len() < 1024);
        }
        ensure!(response.starts_with(b"HTTP/1.1 200"));
        Ok(response)
    }).await?
}

async fn tls_untrusted_wrong_name_accepted() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let acceptor = TlsAcceptor::from(util::create_server_config()?);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await?;
        let mut tls = acceptor.accept(tcp).await?;
        tls.write_all(b"accepted").await?;
        Ok::<(), io::Error>(())
    });
    let tcp = TcpStream::connect(addr).await?;
    let mut tls = timeout(BOUND, connector().connect(ServerName::try_from("wrong-name.example")?, tcp)).await??;
    let mut payload = [0; 8];
    tls.read_exact(&mut payload).await?;
    ensure!(&payload == b"accepted");
    server.await??;
    println!("TLS: self-signed localhost certificate accepted for wrong-name.example");
    Ok(())
}

async fn http_connect_early_data_lost() -> Result<()> {
    let (client, server) = fixture().await?;
    let session = client.create_stream().await?;
    let target = TcpListener::bind("127.0.0.1:0").await?;
    let (mut local, accepted) = pair().await?;
    let request = format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\nHELLO", target.local_addr()?, target.local_addr()?);
    // The complete one-write header plus payload is queued before the handler starts.
    local.write_all(request.as_bytes()).await?;
    let handler = tokio::spawn(http_proxy_probe::audit_handle(accepted, client.clone()));
    read_http_response(&mut local).await?;
    let (mut upstream, _) = timeout(BOUND, target.accept()).await??;
    local.write_all(b"WORLD").await?;
    let mut received = [0; 5];
    timeout(BOUND, upstream.read_exact(&mut received)).await??;
    ensure!(&received == b"WORLD", "observed {:?}", received);
    println!("HTTP CONNECT: queued HELLO lost; target's first bytes = WORLD");
    drop(local);
    session.close().await?;
    timeout(BOUND, handler).await???;
    client.stop_session_pool_cleanup().await;
    server.abort();
    let _ = server.await;
    Ok(())
}

async fn frontend_local_eof_leaks(socks: bool) -> Result<()> {
    let (client, server) = fixture().await?;
    let session = client.create_stream().await?;
    let target = TcpListener::bind("127.0.0.1:0").await?;
    let (mut local, accepted) = pair().await?;
    let handler;
    if socks {
        handler = tokio::spawn(socks5_probe::audit_handle(accepted, client.clone()));
        local.write_all(&[5, 1, 0]).await?;
        let mut auth = [0; 2];
        local.read_exact(&mut auth).await?;
        ensure!(auth == [5, 0]);
        let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
        request.extend_from_slice(&target.local_addr()?.port().to_be_bytes());
        local.write_all(&request).await?;
        let mut response = [0; 10];
        timeout(BOUND, local.read_exact(&mut response)).await??;
        ensure!(response[1] == 0);
    } else {
        local.write_all(format!("CONNECT {} HTTP/1.1\r\n\r\n", target.local_addr()?).as_bytes()).await?;
        handler = tokio::spawn(http_proxy_probe::audit_handle(accepted, client.clone()));
        read_http_response(&mut local).await?;
    }
    let (mut upstream, _) = timeout(BOUND, target.accept()).await??;
    // Also prove established forwarding worked before testing teardown.
    local.write_all(b"alive").await?;
    let mut payload = [0; 5];
    timeout(BOUND, upstream.read_exact(&mut payload)).await??;
    ensure!(&payload == b"alive");
    drop(local);
    let mut byte = [0; 1];
    ensure!(timeout(Duration::from_millis(500), upstream.read(&mut byte)).await.is_err());
    ensure!(session.has_active_streams().await);
    ensure!(!handler.is_finished());
    println!("{} local EOF: target gets no EOF in 500ms; AnyTLS stream still active; handler blocked", if socks {"SOCKS5"} else {"HTTP"});
    session.close().await?;
    timeout(BOUND, handler).await???;
    client.stop_session_pool_cleanup().await;
    server.abort();
    let _ = server.await;
    Ok(())
}

async fn http_remote_eof_not_propagated() -> Result<()> {
    let (client, server) = fixture().await?;
    let session = client.create_stream().await?;
    let target = TcpListener::bind("127.0.0.1:0").await?;
    let (mut local, accepted) = pair().await?;
    local.write_all(format!("CONNECT {} HTTP/1.1\r\n\r\n", target.local_addr()?).as_bytes()).await?;
    let handler = tokio::spawn(http_proxy_probe::audit_handle(accepted, client.clone()));
    read_http_response(&mut local).await?;
    let (upstream, _) = timeout(BOUND, target.accept()).await??;
    drop(upstream);
    timeout(BOUND, async { while session.has_active_streams().await { sleep(Duration::from_millis(5)).await; } }).await?;
    let mut byte = [0; 1];
    ensure!(timeout(Duration::from_millis(500), local.read(&mut byte)).await.is_err());
    ensure!(!handler.is_finished());
    println!("HTTP remote EOF: shared Session processed FIN and removed stream, but local application gets no EOF in 500ms");
    drop(local);
    timeout(BOUND, handler).await???;
    session.close().await?;
    client.stop_session_pool_cleanup().await;
    server.abort();
    let _ = server.await;
    Ok(())
}

async fn client_drop_keeps_session_alive() -> Result<()> {
    let (client, server) = fixture_with_config(anytls_rs::SessionPoolConfig {
        check_interval: Duration::from_millis(50),
        idle_timeout: Duration::from_millis(200),
        min_idle_sessions: 1,
    }).await?;
    let target = TcpListener::bind("127.0.0.1:0").await?;
    let (stream, session) = client.create_proxy_stream(("127.0.0.1".to_owned(), target.local_addr()?.port())).await?;
    let (upstream, _) = timeout(BOUND, target.accept()).await??;
    stream.close();
    timeout(BOUND, async { while session.has_active_streams().await { sleep(Duration::from_millis(5)).await; } }).await?;
    drop(stream);
    drop(upstream);
    let weak = Arc::downgrade(&session);
    drop(session);
    drop(client);
    // More than three configured heartbeat/idle timeouts: replies from the live
    // server keep detached workers alive even after the whole Client was dropped.
    sleep(Duration::from_millis(650)).await;
    let retained = weak.upgrade().ok_or_else(|| anyhow::anyhow!("session did drop"))?;
    ensure!(!retained.is_closed());
    println!("Client Drop: all caller strong references dropped for 650ms (>3*200ms idle/heartbeat timeout), session remains live via background tasks; strong_count={}", Arc::strong_count(&retained));
    retained.close().await?;
    drop(retained);
    timeout(BOUND, async { while weak.upgrade().is_some() { sleep(Duration::from_millis(5)).await; } }).await?;
    server.abort();
    let _ = server.await;
    Ok(())
}

async fn pool_reuses_busy_session() -> Result<()> {
    let session = Arc::new(Session::new_client(tokio::io::empty(), tokio::io::sink(), Arc::new(PaddingFactory::new(b"stop=0").unwrap()), None));
    let pool = SessionPool::new();
    pool.add_idle_session(session.clone()).await;
    let (stream, _) = session.open_stream().await?;
    let a = pool.get_idle_session().await.unwrap();
    let b = pool.get_idle_session().await.unwrap();
    ensure!(a.has_active_streams().await);
    ensure!(Arc::ptr_eq(&a, &b));
    println!("Pool: two idle checkouts return the identical session while stream {} is open", stream.id());
    session.close().await?;
    pool.stop_cleanup_task().await;
    Ok(())
}

#[derive(Clone)]
struct Records(Arc<Mutex<Vec<Vec<u8>>>>);
impl AsyncWrite for Records {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        self.0.lock().unwrap().push(data.to_vec()); Poll::Ready(Ok(data.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> { Poll::Ready(Ok(())) }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> { Poll::Ready(Ok(())) }
}

async fn padding_counter_starts_zero() -> Result<()> {
    let records = Records(Arc::new(Mutex::new(Vec::new())));
    let session = Session::new_client(tokio::io::empty(), records.clone(), Arc::new(PaddingFactory::new(b"stop=3\n0=30-30\n1=60-60\n2=90-90").unwrap()), None);
    for _ in 0..4 { session.write_data_frame(1, Bytes::from_static(b"x")).await?; }
    let sizes: Vec<_> = records.0.lock().unwrap().iter().map(Vec::len).collect();
    ensure!(sizes == vec![30, 60, 90, 8], "got {sizes:?}");
    println!("Padding counter: Session writes {sizes:?}; spec/reference expect [60, 90, 8, 8] after authentication packet0");
    session.close().await?;
    Ok(())
}

fn padding_default_update_fails() -> Result<()> {
    let before = PaddingFactory::default();
    let error = PaddingFactory::update_default(b"stop=2\n0=10-10\n1=123-123").unwrap_err();
    ensure!(PaddingFactory::default().md5() == before.md5());
    println!("Padding update: {error}; default MD5 unchanged = {}", before.md5());
    Ok(())
}

async fn padding_wire_update_ignored() -> Result<()> {
    PaddingFactory::default(); // normal CLI initialization before connecting
    let (reader, mut peer) = tokio::io::duplex(1024);
    let records = Records(Arc::new(Mutex::new(Vec::new())));
    let old_scheme = Arc::new(PaddingFactory::new(b"stop=8\n0=30-30\n1=60-60").unwrap());
    let session = Arc::new(Session::new_client(reader, records.clone(), old_scheme, None));
    let recv_session = session.clone();
    let recv = tokio::spawn(async move { recv_session.recv_loop().await });
    let scheme = b"stop=8\n0=200-200\n1=240-240";
    let mut wire = vec![6, 0, 0, 0, 0];
    wire.extend_from_slice(&(scheme.len() as u16).to_be_bytes());
    wire.extend_from_slice(scheme);
    wire.extend_from_slice(&[8, 0, 0, 0, 0, 0, 0]); // ordered barrier: HeartRequest after update
    peer.write_all(&wire).await?;
    timeout(BOUND, async { while records.0.lock().unwrap().is_empty() { tokio::task::yield_now().await; } }).await?;
    let sizes: Vec<_> = records.0.lock().unwrap().iter().map(Vec::len).collect();
    ensure!(sizes == [30]);
    println!("Wire UpdatePaddingScheme: ordered HeartResponse still uses old 30-byte scheme (new scheme would use 200/240)");
    session.close().await?;
    let _ = timeout(BOUND, recv).await?;
    Ok(())
}

async fn socks_unknown_commands_connect() -> Result<()> {
    for command in [2u8, 3, 0x99] {
        let (client, server) = fixture().await?;
        let session = client.create_stream().await?;
        let target = TcpListener::bind("127.0.0.1:0").await?;
        let (mut local, accepted) = pair().await?;
        let handler = tokio::spawn(socks5_probe::audit_handle(accepted, client.clone()));
        local.write_all(&[5, 1, 0]).await?;
        let mut auth = [0; 2];
        local.read_exact(&mut auth).await?;
        let mut request = vec![5, command, 0, 1, 127, 0, 0, 1];
        request.extend_from_slice(&target.local_addr()?.port().to_be_bytes());
        local.write_all(&request).await?;
        let mut response = [0; 10];
        timeout(BOUND, local.read_exact(&mut response)).await??;
        ensure!(response[1] == 0);
        let (upstream, _) = timeout(BOUND, target.accept()).await??;
        println!("SOCKS5 CMD=0x{command:02x}: returns success REP=0 and actually establishes TCP CONNECT");
        drop(local);
        drop(upstream);
        session.close().await?;
        timeout(BOUND, handler).await???;
        client.stop_session_pool_cleanup().await;
        server.abort();
        let _ = server.await;
    }
    Ok(())
}

async fn v1_peer_never_accepted_without_synack() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let padding = Arc::new(PaddingFactory::new(b"stop=0").unwrap());
    let server_padding = padding.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let acceptor = TlsAcceptor::from(util::create_server_config()?);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await?;
        let mut tls = acceptor.accept(tcp).await?;
        util::authenticate_client(&mut tls, &util::hash_password("audit-only"), &server_padding).await?;
        let mut ready = Some(ready_tx);
        loop {
            let mut header = [0; 7];
            tls.read_exact(&mut header).await?;
            let length = u16::from_be_bytes([header[5], header[6]]) as usize;
            let mut data = vec![0; length];
            tls.read_exact(&mut data).await?;
            if header[0] == 2 && ready.is_some() {
                // A v1 peer sends proxied bytes without v2 ServerSettings/SYNACK.
                let mut response = vec![2];
                response.extend_from_slice(&header[1..5]);
                response.extend_from_slice(&8u16.to_be_bytes());
                response.extend_from_slice(b"v1-ready");
                tls.write_all(&response).await?;
                tls.flush().await?;
                let _ = ready.take().unwrap().send(());
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });
    let client = Arc::new(Client::new("audit-only", addr.to_string(), ServerName::try_from("localhost")?, connector(), padding));
    let opening_client = client.clone();
    let opening = tokio::spawn(async move { opening_client.create_proxy_stream(("example.com".to_string(), 80)).await });
    timeout(BOUND, ready_rx).await??;
    let error = match timeout(Duration::from_secs(32), opening).await?? {
        Ok(_) => anyhow::bail!("v1 peer unexpectedly succeeded"),
        Err(error) => error.to_string(),
    };
    ensure!(error.contains("SYNACK timeout after 30s"));
    println!("v1 peer: sent PSH v1-ready successfully, but create_proxy_stream returns {error}");
    let session = client.create_stream().await?;
    session.close().await?;
    client.stop_session_pool_cleanup().await;
    server.abort();
    let _ = server.await;
    Ok(())
}

async fn tls_handshake_has_no_deadline() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let client = Arc::new(Client::new("audit-only", addr.to_string(), ServerName::try_from("localhost")?, connector(), Arc::new(PaddingFactory::new(b"stop=0").unwrap())));
    let opening_client = client.clone();
    let opening = tokio::spawn(async move { opening_client.create_proxy_stream(("example.com".to_string(), 80)).await });
    let (mut accepted, _) = timeout(BOUND, listener.accept()).await??;
    let mut first = [0; 1];
    timeout(BOUND, accepted.read_exact(&mut first)).await??;
    ensure!(first[0] == 0x16); // TLS handshake record / ClientHello
    sleep(Duration::from_secs(31)).await;
    ensure!(!opening.is_finished());
    println!("TLS stall: TCP accepted and ClientHello received, but create_proxy_stream remains pending beyond 31s (SYNACK timeout never started)");
    opening.abort();
    let _ = opening.await;
    client.stop_session_pool_cleanup().await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tls_untrusted_wrong_name_accepted().await?;
    http_connect_early_data_lost().await?;
    frontend_local_eof_leaks(false).await?;
    frontend_local_eof_leaks(true).await?;
    http_remote_eof_not_propagated().await?;
    client_drop_keeps_session_alive().await?;
    pool_reuses_busy_session().await?;
    padding_counter_starts_zero().await?;
    padding_default_update_fails()?;
    padding_wire_update_ignored().await?;
    socks_unknown_commands_connect().await?;
    let (v1, tls) = tokio::join!(v1_peer_never_accepted_without_synack(), tls_handshake_has_no_deadline());
    v1?;
    tls?;
    println!("All bounded client-audit probes reproduced the suspected current behavior.");
    Ok(())
}
