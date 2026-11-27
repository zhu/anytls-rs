//! A failed rotation must leave the existing certificate usable by TLS clients.

use anytls_rs::util::{CertReloader, CertReloaderConfig};
use rcgen::{CertificateParams, KeyPair, date_time_ymd};
use rustls::pki_types::{CertificateDer, ServerName};
use std::sync::Arc;
use std::time::Duration;
use tokio_rustls::TlsConnector;

fn write_certificate(
    config: &CertReloaderConfig,
    not_before_year: i32,
    not_after_year: i32,
) -> CertificateDer<'static> {
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.not_before = date_time_ymd(not_before_year, 1, 1);
    params.not_after = date_time_ymd(not_after_year, 1, 1);
    let key = KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    std::fs::write(&config.cert_path, certificate.pem()).unwrap();
    std::fs::write(&config.key_path, key.serialize_pem()).unwrap();
    certificate.der().clone()
}

async fn assert_served_certificate(reloader: &CertReloader, expected: CertificateDer<'static>) {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(expected.clone()).unwrap();
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let acceptor = reloader.get_acceptor();
    let (client, server) = tokio::io::duplex(16 * 1024);
    let (client, server) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(
            connector.connect(ServerName::try_from("localhost").unwrap(), client),
            acceptor.accept(server),
        )
    })
    .await
    .expect("TLS handshake timed out");
    let client = client.expect("client must still verify the active certificate");
    server.expect("server TLS handshake failed");
    assert_eq!(client.get_ref().1.peer_certificates().unwrap(), &[expected]);
}

async fn rejected_reload_keeps_active_certificate(
    not_before_year: i32,
    not_after_year: i32,
    error_message: &str,
) {
    let directory = tempfile::tempdir().unwrap();
    let config = CertReloaderConfig {
        cert_path: directory.path().join("cert.pem"),
        key_path: directory.path().join("key.pem"),
        watch_enabled: false,
        ..Default::default()
    };
    let year = i32::from(
        jiff::Timestamp::now()
            .to_zoned_datetime(jiff::tz::TimeZone::UTC)
            .year(),
    );
    let original = write_certificate(&config, year - 1, year + 5);
    let reloader = CertReloader::new(config.clone()).unwrap();
    let original_acceptor = reloader.get_acceptor();
    let original_info = reloader.get_cert_info().unwrap();

    write_certificate(&config, not_before_year, not_after_year);
    let error = reloader.reload().expect_err("invalid rotation must fail");
    assert!(error.to_string().contains(error_message), "{error}");
    assert!(Arc::ptr_eq(&original_acceptor, &reloader.get_acceptor()));
    assert_eq!(
        reloader.get_cert_info().unwrap().serial_number,
        original_info.serial_number
    );
    assert_eq!(reloader.get_reload_count(), 0);
    assert!(reloader.get_last_reload().is_none());
    assert_served_certificate(&reloader, original).await;

    // Rejection must not prevent a later valid renewal from being installed.
    let replacement = write_certificate(&config, year - 1, year + 6);
    reloader.reload().unwrap();
    assert!(!Arc::ptr_eq(&original_acceptor, &reloader.get_acceptor()));
    assert_eq!(reloader.get_reload_count(), 1);
    assert!(reloader.get_last_reload().is_some());
    assert_served_certificate(&reloader, replacement).await;
}

#[tokio::test]
async fn future_dated_reload_preserves_the_current_certificate() {
    let year = i32::from(
        jiff::Timestamp::now()
            .to_zoned_datetime(jiff::tz::TimeZone::UTC)
            .year(),
    );
    rejected_reload_keeps_active_certificate(year + 1, year + 2, "not valid yet").await;
}

#[tokio::test]
async fn expired_reload_preserves_the_current_certificate() {
    let year = i32::from(
        jiff::Timestamp::now()
            .to_zoned_datetime(jiff::tz::TimeZone::UTC)
            .year(),
    );
    rejected_reload_keeps_active_certificate(year - 2, year - 1, "has expired").await;
}
