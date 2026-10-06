use super::NotificationError;
use async_io::Async;
use futures_lite::future;
use futures_rustls::{TlsConnector, client::TlsStream};
use rustls::{
    ClientConfig, client::danger::ServerCertVerifier, crypto::CryptoProvider, pki_types::ServerName,
};
use std::{
    future::Future,
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    sync::Arc,
    time::{Duration, Instant},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn connect(
    host: &str,
    port: u16,
) -> Result<TlsStream<Async<TcpStream>>, NotificationError> {
    let name =
        ServerName::try_from(host.to_owned()).map_err(|_| NotificationError::InvalidInput)?;
    if port == 0 {
        return Err(NotificationError::InvalidInput);
    }
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let host = host.to_owned();
    // Native trust evaluation and DNS resolution can block. Keep the caller's
    // deadline independent of those operations; no MQTT data is sent here.
    let connection = blocking::unblock(move || {
        future::block_on(async {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
                .map_err(|_| NotificationError::Tls)?;
            let config = client_config(provider, Arc::new(verifier))?;
            let addresses = (host.as_str(), port)
                .to_socket_addrs()
                .map_err(|_| NotificationError::Io)?
                .collect();
            with_deadline(connect_addresses(name, addresses, config), deadline).await
        })
    });
    with_deadline(connection, deadline).await
}

fn client_config(
    provider: Arc<CryptoProvider>,
    verifier: Arc<dyn ServerCertVerifier>,
) -> Result<Arc<ClientConfig>, NotificationError> {
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| NotificationError::Tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

async fn connect_addresses(
    name: ServerName<'static>,
    addresses: Vec<SocketAddr>,
    config: Arc<ClientConfig>,
) -> Result<TlsStream<Async<TcpStream>>, NotificationError> {
    for address in addresses {
        if let Ok(socket) = Async::<TcpStream>::connect(address).await {
            return TlsConnector::from(config)
                .connect(name, socket)
                .await
                .map_err(|_| NotificationError::Tls);
        }
    }
    Err(NotificationError::Io)
}

async fn with_deadline<T>(
    operation: impl Future<Output = Result<T, NotificationError>>,
    deadline: Instant,
) -> Result<T, NotificationError> {
    if Instant::now() >= deadline {
        return Err(NotificationError::Timeout);
    }
    future::race(operation, async {
        async_io::Timer::at(deadline).await;
        Err(NotificationError::Timeout)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::{
        future::{self, block_on},
        io::{AsyncReadExt, AsyncWriteExt},
    };
    use futures_rustls::TlsAcceptor;
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    use rustls::{
        RootCertStore, ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        server::WebPkiClientVerifier,
    };
    use std::net::TcpListener;
    use time::OffsetDateTime;

    const HOST: &str = "cn-ha.mqtt.io.mi.com";

    struct Fixture {
        root: CertificateDer<'static>,
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    }

    impl Fixture {
        fn new(not_before: i64, not_after: i64, usage: ExtendedKeyUsagePurpose) -> Self {
            let root_key = KeyPair::generate().unwrap();
            let mut root_params = CertificateParams::new(Vec::<String>::new()).unwrap();
            root_params
                .distinguished_name
                .push(DnType::CommonName, "cloud test root");
            root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            root_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            let root = root_params.self_signed(&root_key).unwrap().der().clone();
            let root_issuer = Issuer::new(root_params, root_key);

            let intermediate_key = KeyPair::generate().unwrap();
            let mut intermediate_params = CertificateParams::new(Vec::<String>::new()).unwrap();
            intermediate_params
                .distinguished_name
                .push(DnType::CommonName, "cloud test intermediate");
            intermediate_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
            intermediate_params.key_usages =
                vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            let intermediate = intermediate_params
                .signed_by(&intermediate_key, &root_issuer)
                .unwrap()
                .der()
                .clone();
            let intermediate_issuer = Issuer::new(intermediate_params, intermediate_key);

            let now = OffsetDateTime::now_utc();
            let server_key = KeyPair::generate().unwrap();
            let mut server_params = CertificateParams::new(vec![HOST.to_owned()]).unwrap();
            server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            server_params.extended_key_usages = vec![usage];
            server_params.not_before = now + time::Duration::seconds(not_before);
            server_params.not_after = now + time::Duration::seconds(not_after);
            let server = server_params
                .signed_by(&server_key, &intermediate_issuer)
                .unwrap()
                .der()
                .clone();
            Self {
                root,
                chain: vec![server, intermediate],
                key: PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
            }
        }

        fn valid() -> Self {
            Self::new(-120, 3600, ExtendedKeyUsagePurpose::ServerAuth)
        }

        fn client_config(&self) -> Arc<ClientConfig> {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let verifier = rustls_platform_verifier::Verifier::new_with_extra_roots(
                [self.root.clone()],
                provider.clone(),
            )
            .unwrap();
            client_config(provider, Arc::new(verifier)).unwrap()
        }

        fn server_config(&self, tls12: bool) -> Arc<ServerConfig> {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let mut roots = RootCertStore::empty();
            roots.add(self.root.clone()).unwrap();
            let verifier =
                WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                    .allow_unauthenticated()
                    .build()
                    .unwrap();
            let versions: &[&rustls::SupportedProtocolVersion] = if tls12 {
                &[&rustls::version::TLS12]
            } else {
                &[&rustls::version::TLS13]
            };
            Arc::new(
                ServerConfig::builder_with_provider(provider)
                    .with_protocol_versions(versions)
                    .unwrap()
                    .with_client_cert_verifier(verifier)
                    .with_single_cert(self.chain.clone(), self.key.clone_key())
                    .unwrap(),
            )
        }
    }

    async fn exchange(
        server: &Fixture,
        trusted: &Fixture,
        host: &str,
        tls12: bool,
    ) -> Result<(), NotificationError> {
        let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let address = listener.get_ref().local_addr().unwrap();
        let acceptor = TlsAcceptor::from(server.server_config(tls12));
        let accept = async {
            let (socket, _) = listener.accept().await.unwrap();
            if let Ok(mut stream) = acceptor.accept(socket).await {
                assert!(stream.get_ref().1.peer_certificates().is_none());
                let mut byte = [0];
                if stream.read_exact(&mut byte).await.is_ok() {
                    let _ = stream.write_all(&byte).await;
                    let _ = stream.flush().await;
                }
            }
        };
        let client = async {
            let mut stream = connect_addresses(
                ServerName::try_from(host.to_owned()).unwrap(),
                vec![address],
                trusted.client_config(),
            )
            .await?;
            assert_eq!(
                stream.get_ref().1.protocol_version(),
                Some(if tls12 {
                    rustls::ProtocolVersion::TLSv1_2
                } else {
                    rustls::ProtocolVersion::TLSv1_3
                }),
            );
            stream
                .write_all(&[42])
                .await
                .map_err(|_| NotificationError::Io)?;
            stream.flush().await.map_err(|_| NotificationError::Io)?;
            let mut byte = [0];
            stream
                .read_exact(&mut byte)
                .await
                .map_err(|_| NotificationError::Io)?;
            assert_eq!(byte, [42]);
            Ok(())
        };
        with_deadline(
            async { future::zip(accept, client).await.1 },
            Instant::now() + Duration::from_secs(3),
        )
        .await
    }

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
}
