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
    block_on(async {
        let (connected, accepted) = flume::bounded(1);
        let connect = connect_addresses(
            ServerName::try_from(HOST.to_owned()).unwrap(),
            vec![address],
            server.client_config(),
        );
        let handshake = async {
            let mut connect = Box::pin(connect);
            assert!(future::poll_once(connect.as_mut()).await.is_none());
            accepted.recv_async().await.unwrap();
            let result = with_deadline(connect, Instant::now() + Duration::from_millis(50)).await;
            assert_eq!(result.unwrap_err(), NotificationError::Timeout);
        };
        let hold_connection = async {
            let (_socket, _) = listener.accept().await.unwrap();
            connected.send_async(()).await.unwrap();
            future::pending::<()>().await
        };
        future::or(future::or(handshake, hold_connection), async {
            async_io::Timer::after(Duration::from_secs(10)).await;
            panic!("stalled TLS handshake outlived its deadline");
        })
        .await;
    });
}

#[test]
fn cloud_tls_deadline_returns_while_blocking_resolution_is_still_running() {
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, entered) = flume::bounded(1);
    let (finished, done) = flume::bounded(1);
    let result = block_on(async {
        let mut resolver = Box::pin(blocking::unblock(move || {
            started.send(()).unwrap();
            let _ = blocked.recv_timeout(Duration::from_secs(10));
            finished.send(()).unwrap();
            Ok::<(), NotificationError>(())
        }));
        assert!(future::poll_once(resolver.as_mut()).await.is_none());
        entered.recv_async().await.unwrap();
        with_deadline(resolver, Instant::now() + Duration::from_millis(50)).await
    });
    assert_eq!(result, Err(NotificationError::Timeout));
    assert_eq!(done.try_recv(), Err(flume::TryRecvError::Empty));
    release.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(10)).unwrap();
}
