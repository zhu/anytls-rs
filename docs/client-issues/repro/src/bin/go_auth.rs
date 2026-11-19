use anyhow::{Result, ensure};
use anytls_rs::{Client, PaddingFactory, Session, util};
use bytes::Bytes;
use std::{process::{Child, Command, Stdio}, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, net::{TcpListener, TcpStream}, time::{sleep, timeout}};
use tokio_rustls::{TlsConnector, rustls::pki_types::ServerName};

struct Kill(Child);
impl Drop for Kill { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

#[tokio::main]
async fn main() -> Result<()> {
    let reserve = TcpListener::bind("127.0.0.1:0").await?;
    let address = reserve.local_addr()?;
    drop(reserve);
    let log = std::fs::File::create("go-auth-server.log")?;
    let go_server = std::env::var("ANYTLS_GO_SERVER").unwrap_or_else(|_| "anytls-go-server".to_owned());
    let _server = Kill(Command::new(go_server)
        .args(["-l", &address.to_string(), "-p", "audit-only"])
        .env("LOG_LEVEL", "debug").stdout(Stdio::null()).stderr(Stdio::from(log)).spawn()?);
    timeout(Duration::from_secs(3), async {
        loop { match TcpStream::connect(address).await { Ok(tcp) => { drop(tcp); break; }, Err(_) => sleep(Duration::from_millis(10)).await } }
    }).await?;
    let target = TcpListener::bind("127.0.0.1:0").await?;
    let padding = PaddingFactory::default();
    let connector = Arc::new(TlsConnector::from(util::create_client_config()?));
    let client = Client::new("audit-only", address.to_string(), ServerName::try_from("localhost")?, connector.clone(), padding.clone());
    let error = match timeout(Duration::from_secs(3), client.create_proxy_stream(("127.0.0.1".to_owned(), target.local_addr()?.port()))).await? {
        Ok(_) => anyhow::bail!("stock Rust client unexpectedly connected to Go server"),
        Err(error) => error.to_string(),
    };
    ensure!(timeout(Duration::from_millis(100), target.accept()).await.is_err());
    println!("Stock Rust Client -> stock anytls-go: {error}; target never connected");
    client.stop_session_pool_cleanup().await;

    // Control: same Rust TLS/session stack, only authentication is gathered into one write.
    let tcp = TcpStream::connect(address).await?;
    let mut tls = connector.connect(ServerName::try_from("localhost")?, tcp).await?;
    let mut auth = util::hash_password("audit-only").to_vec();
    auth.extend_from_slice(&30u16.to_be_bytes());
    auth.extend_from_slice(&[0; 30]);
    tls.write_all(&auth).await?;
    tls.flush().await?;
    let (reader, writer) = tokio::io::split(tls);
    let session = Arc::new(Session::new_client(reader, writer, padding, None));
    session.clone().start_client().await?;
    let (stream, synack) = session.open_stream().await?;
    let mut destination = vec![1, 127, 0, 0, 1];
    destination.extend_from_slice(&target.local_addr()?.port().to_be_bytes());
    session.disable_buffering();
    session.write_data_frame(stream.id(), Bytes::from(destination)).await?;
    timeout(Duration::from_secs(3), synack).await???;
    let (_accepted, _) = timeout(Duration::from_secs(3), target.accept()).await??;
    println!("Control with one gathered 64-byte authentication write: SYNACK succeeds and target TCP connects");
    session.close().await?;
    Ok(())
}
