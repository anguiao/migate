use super::CentralError;
use crate::{
    storage::XiaomiRecord,
    xiaomi::certificate::{XIAOMI_CA_PEM, validate_certificate},
};
use async_io::Async;
use futures_lite::future;
use futures_rustls::{TlsConnector, client::TlsStream};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        verify_server_cert_signed_by_trust_anchor,
    },
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject},
    server::ParsedCertificate,
};
use std::{
    net::{SocketAddr, TcpStream},
    sync::Arc,
    time::Duration,
};

/// Connect to a currently discovered local endpoint using the saved client
/// identity. Cloud authentication status does not replace mutual TLS access.
pub async fn connect_tls(
    address: SocketAddr,
    record: &XiaomiRecord,
) -> Result<TlsStream<Async<TcpStream>>, CentralError> {
    connect(address, record, XIAOMI_CA_PEM).await
}

pub(super) async fn connect(
    address: SocketAddr,
    record: &XiaomiRecord,
    ca_pem: &str,
) -> Result<TlsStream<Async<TcpStream>>, CentralError> {
    if address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
        return Err(CentralError::InvalidInput);
    }
    let config = client_config(record, ca_pem)?;
    future::race(
        async {
            let socket = Async::<TcpStream>::connect(address)
                .await
                .map_err(|_| CentralError::Io)?;
            TlsConnector::from(config)
                .connect(ServerName::IpAddress(address.ip().into()), socket)
                .await
                .map_err(|_| CentralError::Tls)
        },
        async {
            async_io::Timer::after(Duration::from_secs(5)).await;
            Err(CentralError::Timeout)
        },
    )
    .await
}

pub(super) fn client_config(
    record: &XiaomiRecord,
    ca_pem: &str,
) -> Result<Arc<ClientConfig>, CentralError> {
    let validity = validate_certificate(
        &record.uid,
        &record.virtual_did,
        &record.private_key_pem,
        &record.certificate_pem,
    )
    .map_err(|_| CentralError::InvalidCredentials)?;
    if !validity.currently_valid(time::OffsetDateTime::now_utc().unix_timestamp()) {
        return Err(CentralError::InvalidCredentials);
    }
    let mut roots = RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        roots
            .add(certificate.map_err(|_| CentralError::InvalidCredentials)?)
            .map_err(|_| CentralError::InvalidCredentials)?;
    }
    if roots.is_empty() {
        return Err(CentralError::InvalidCredentials);
    }
    let certificate = CertificateDer::from_pem_slice(record.certificate_pem.as_bytes())
        .map_err(|_| CentralError::InvalidCredentials)?;
    let key = PrivateKeyDer::from_pem_slice(record.private_key_pem.as_bytes())
        .map_err(|_| CentralError::InvalidCredentials)?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = Arc::new(GatewayVerifier {
        roots,
        provider: provider.clone(),
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| CentralError::Tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![certificate], key)
        .map_err(|_| CentralError::InvalidCredentials)?;
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct GatewayVerifier {
    roots: RootCertStore,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for GatewayVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let certificate = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &certificate,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        // The official central protocol disables hostname checks only. Chain,
        // validity, server usage, and handshake signatures remain verified.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
