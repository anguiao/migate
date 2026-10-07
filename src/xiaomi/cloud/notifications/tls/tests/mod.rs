use super::*;
use futures_lite::{
    future::{self, block_on},
    io::{AsyncReadExt, AsyncWriteExt},
};
use futures_rustls::TlsAcceptor;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::WebPkiClientVerifier,
};
use std::net::TcpListener;
use time::OffsetDateTime;

use support::*;

mod support;

#[test]
fn cloud_tls_verifies_trusted_chain_and_dns_without_client_identity() {
    let server = Fixture::valid();
    block_on(exchange(&server, &server, HOST, false)).unwrap();
    block_on(exchange(&server, &server, HOST, true)).unwrap();
}

#[test]
fn cloud_tls_rejects_wrong_dns_untrusted_expired_and_wrong_usage() {
    let server = Fixture::valid();
    assert_eq!(
        block_on(exchange(&server, &server, "unrelated.invalid", false)),
        Err(NotificationError::Tls)
    );
    let unrelated = Fixture::valid();
    assert_eq!(
        block_on(exchange(&server, &unrelated, HOST, false)),
        Err(NotificationError::Tls)
    );
    let expired = Fixture::new(-120, -60, ExtendedKeyUsagePurpose::ServerAuth);
    assert_eq!(
        block_on(exchange(&expired, &expired, HOST, false)),
        Err(NotificationError::Tls)
    );
    let future = Fixture::new(60, 3600, ExtendedKeyUsagePurpose::ServerAuth);
    assert_eq!(
        block_on(exchange(&future, &future, HOST, false)),
        Err(NotificationError::Tls)
    );
    let client_usage = Fixture::new(-120, 3600, ExtendedKeyUsagePurpose::ClientAuth);
    assert_eq!(
        block_on(exchange(&client_usage, &client_usage, HOST, false)),
        Err(NotificationError::Tls)
    );
}

#[test]
fn cloud_tls_rejects_missing_intermediate() {
    let mut server = Fixture::valid();
    server.chain.truncate(1);
    assert_eq!(
        block_on(exchange(&server, &server, HOST, false)),
        Err(NotificationError::Tls)
    );
}

#[test]
fn cloud_tls_rejects_invalid_endpoint_before_connecting() {
    for (host, port) in [("", 8883), ("https://invalid", 8883), (HOST, 0)] {
        assert_eq!(
            block_on(connect(host, port)).unwrap_err(),
            NotificationError::InvalidInput
        );
    }
}

#[test]
fn cloud_tls_deadline_covers_a_stalled_handshake() {
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    let address = listener.get_ref().local_addr().unwrap();
    let server = Fixture::valid();
    let start = Instant::now();
    let result = block_on(with_deadline(
        connect_addresses(
            ServerName::try_from(HOST.to_owned()).unwrap(),
            vec![address],
            server.client_config(),
        ),
        start + Duration::from_millis(50),
    ));
    assert_eq!(result.unwrap_err(), NotificationError::Timeout);
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn cloud_tls_deadline_returns_while_blocking_resolution_is_still_running() {
    let (release, blocked) = std::sync::mpsc::channel();
    let start = Instant::now();
    let result = block_on(with_deadline(
        blocking::unblock(move || {
            blocked.recv().unwrap();
            Ok::<(), NotificationError>(())
        }),
        start + Duration::from_millis(50),
    ));
    assert_eq!(result, Err(NotificationError::Timeout));
    assert!(start.elapsed() < Duration::from_secs(1));
    release.send(()).unwrap();
}
