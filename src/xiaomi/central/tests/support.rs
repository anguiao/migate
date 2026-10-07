use super::super::*;
use crate::{
    storage::{TokenSet, XiaomiRecord},
    xiaomi::certificate::ClientIdentity,
};
use async_io::Async;
use futures_lite::{
    future,
    io::{AsyncReadExt, AsyncWriteExt},
};
use futures_rustls::TlsAcceptor;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::WebPkiClientVerifier,
};
use std::{net::TcpListener, sync::Arc, time::Duration};
use time::OffsetDateTime;

pub(crate) struct TlsFixture {
    pub(crate) ca_pem: String,
    server_pem: String,
    server_key: String,
    pub(crate) credentials: XiaomiRecord,
    pub(super) force_tls12: bool,
}

impl TlsFixture {
    pub(crate) fn new(expired_server: bool) -> Self {
        Self::server_case(
            -120,
            if expired_server { -60 } else { 3600 },
            ExtendedKeyUsagePurpose::ServerAuth,
        )
    }

    pub(super) fn server_case(
        not_before: i64,
        not_after: i64,
        usage: ExtendedKeyUsagePurpose,
    ) -> Self {
        let now = OffsetDateTime::now_utc();
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "isolated test root");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_pem = ca_params.self_signed(&ca_key).unwrap().pem();
        let issuer = Issuer::new(ca_params, ca_key);
        let server_key = KeyPair::generate().unwrap();
        let mut server_params = CertificateParams::new(vec!["unrelated.invalid".into()]).unwrap();
        server_params.extended_key_usages = vec![usage];
        server_params.not_before = now + time::Duration::seconds(not_before);
        server_params.not_after = now + time::Duration::seconds(not_after);
        let server_pem = server_params.signed_by(&server_key, &issuer).unwrap().pem();
        let identity = ClientIdentity::generate("isolated-account").unwrap();
        let mut csr = CertificateSigningRequestParams::from_pem(&identity.csr_pem).unwrap();
        csr.params.not_before = now - time::Duration::seconds(60);
        csr.params.not_after = now + time::Duration::seconds(3600);
        csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let certificate_pem = csr.signed_by(&issuer).unwrap().pem();
        Self {
            ca_pem,
            server_pem,
            server_key: server_key.serialize_pem(),
            force_tls12: false,
            credentials: XiaomiRecord {
                uid: "isolated-account".into(),
                region: "cn".into(),
                oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
                redirect_uri:
                    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                        .into(),
                tokens: TokenSet {
                    access_token: "unused".into(),
                    refresh_token: "unused".into(),
                    expires_at: now.unix_timestamp() + 3600,
                    refresh_at: now.unix_timestamp() + 3000,
                },
                virtual_did: identity.virtual_did,
                private_key_pem: identity.private_key_pem,
                certificate_pem,
            },
        }
    }

    pub(crate) fn server_config(&self) -> Arc<ServerConfig> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(self.ca_pem.as_bytes()).unwrap())
            .unwrap();
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()
                .unwrap();
        let versions: &[&rustls::SupportedProtocolVersion] = if self.force_tls12 {
            &[&rustls::version::TLS12]
        } else {
            rustls::DEFAULT_VERSIONS
        };
        Arc::new(
            ServerConfig::builder_with_provider(provider)
                .with_protocol_versions(versions)
                .unwrap()
                .with_client_cert_verifier(verifier)
                .with_single_cert(
                    vec![CertificateDer::from_pem_slice(self.server_pem.as_bytes()).unwrap()],
                    PrivateKeyDer::from_pem_slice(self.server_key.as_bytes()).unwrap(),
                )
                .unwrap(),
        )
    }
}

pub(super) async fn exchange_tls(
    server: &TlsFixture,
    credentials: &XiaomiRecord,
    trusted_ca: &str,
) -> Result<(), CentralError> {
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    let address = listener.get_ref().local_addr().unwrap();
    let acceptor = TlsAcceptor::from(server.server_config());
    let accept = async {
        let (socket, _) = listener.accept().await.unwrap();
        if let Ok(mut stream) = acceptor.accept(socket).await {
            let mut byte = [0];
            if stream.read_exact(&mut byte).await.is_ok() {
                let _ = stream.write_all(&byte).await;
                let _ = stream.flush().await;
            }
        }
    };
    let client = async {
        let mut stream = tls::connect(address, credentials, trusted_ca).await?;
        if server.force_tls12 {
            assert_eq!(
                stream.get_ref().1.protocol_version(),
                Some(rustls::ProtocolVersion::TLSv1_2)
            );
        }
        stream
            .write_all(&[42])
            .await
            .map_err(|_| CentralError::Tls)?;
        stream.flush().await.map_err(|_| CentralError::Tls)?;
        let mut byte = [0];
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|_| CentralError::Tls)?;
        assert_eq!(byte, [42]);
        Ok(())
    };
    future::race(async { future::zip(accept, client).await.1 }, async {
        async_io::Timer::after(Duration::from_secs(3)).await;
        Err(CentralError::Timeout)
    })
    .await
}

pub(crate) use super::super::session::tests::support::Broker;

pub(crate) async fn connect(
    address: std::net::SocketAddr,
    credentials: &crate::storage::XiaomiRecord,
    ca: &str,
    request: std::time::Duration,
) -> Result<(SessionHandle, SessionDriver), CentralError> {
    let stream = tls::connect(address, credentials, ca).await?;
    session::start(
        Box::new(stream),
        &credentials.virtual_did,
        session::Timing {
            request,
            keepalive: None,
        },
    )
    .await
}
