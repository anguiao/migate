use super::super::{CentralError, tls};
use super::support::{TlsFixture, exchange_tls};
use futures_lite::future::block_on;
use rcgen::ExtendedKeyUsagePurpose;

#[test]
fn gateway_tls_authenticates_both_peers_and_only_skips_hostname() {
    let server = TlsFixture::new(false);
    block_on(exchange_tls(&server, &server.credentials, &server.ca_pem)).unwrap();
    let mut legacy = TlsFixture::new(false);
    legacy.force_tls12 = true;
    block_on(exchange_tls(&legacy, &legacy.credentials, &legacy.ca_pem)).unwrap();
    let other = TlsFixture::new(false);
    assert_eq!(
        block_on(exchange_tls(&server, &server.credentials, &other.ca_pem)),
        Err(CentralError::Tls)
    );
    let expired = TlsFixture::new(true);
    assert_eq!(
        block_on(exchange_tls(
            &expired,
            &expired.credentials,
            &expired.ca_pem
        )),
        Err(CentralError::Tls)
    );
    let future = TlsFixture::server_case(60, 3600, ExtendedKeyUsagePurpose::ServerAuth);
    assert_eq!(
        block_on(exchange_tls(&future, &future.credentials, &future.ca_pem)),
        Err(CentralError::Tls)
    );
    let wrong_usage = TlsFixture::server_case(-60, 3600, ExtendedKeyUsagePurpose::ClientAuth);
    assert_eq!(
        block_on(exchange_tls(
            &wrong_usage,
            &wrong_usage.credentials,
            &wrong_usage.ca_pem
        )),
        Err(CentralError::Tls)
    );
    assert_eq!(
        block_on(exchange_tls(&server, &other.credentials, &server.ca_pem)),
        Err(CentralError::Tls)
    );
    let mut wrong_identity = server.credentials.clone();
    wrong_identity.uid = "wrong-account".into();
    assert_eq!(
        tls::client_config(&wrong_identity, &server.ca_pem).unwrap_err(),
        CentralError::InvalidCredentials
    );
}
