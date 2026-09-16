use super::{MqttError, MqttErrorKind};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        verify_server_cert_signed_by_trust_anchor,
    },
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    server::ParsedCertificate,
};
use std::{fmt, io::BufReader, sync::Arc};

pub struct CloudTlsConfig {
    config: Arc<ClientConfig>,
    server_name: String,
}

impl fmt::Debug for CloudTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CloudTlsConfig")
            .finish_non_exhaustive()
    }
}

impl CloudTlsConfig {
    pub fn webpki(server_name: &str) -> Result<Self, MqttError> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::with_roots(server_name, roots)
    }

    pub fn with_ca_pem(server_name: &str, ca_pem: &str) -> Result<Self, MqttError> {
        Self::with_roots(server_name, roots_from_pem(ca_pem, "configure cloud TLS")?)
    }

    fn with_roots(server_name: &str, roots: RootCertStore) -> Result<Self, MqttError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| MqttError::new("configure cloud TLS", MqttErrorKind::Protocol))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        ServerName::try_from(server_name.to_owned())
            .map_err(|_| MqttError::new("configure cloud TLS", MqttErrorKind::InvalidInput))?;
        Ok(Self {
            config: Arc::new(config),
            server_name: server_name.to_owned(),
        })
    }

    pub fn client_config_for(&self, host: &str) -> Result<Arc<ClientConfig>, MqttError> {
        if host != self.server_name {
            return Err(MqttError::new(
                "configure cloud TLS",
                MqttErrorKind::InvalidInput,
            ));
        }
        Ok(self.config.clone())
    }
}

pub struct GatewayTlsConfig {
    config: Arc<ClientConfig>,
}

impl fmt::Debug for GatewayTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayTlsConfig")
            .finish_non_exhaustive()
    }
}

impl GatewayTlsConfig {
    pub fn new(
        ca_pem: &str,
        certificate_pem: &str,
        private_key_pem: &str,
    ) -> Result<Self, MqttError> {
        let roots = Arc::new(roots_from_pem(ca_pem, "configure gateway TLS")?);
        let certificates = certificates_from_pem(certificate_pem, "configure gateway TLS")?;
        let key = private_key_from_pem(private_key_pem, "configure gateway TLS")?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(LocalGatewayVerifier {
            roots,
            provider: provider.clone(),
        });
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| MqttError::new("configure gateway TLS", MqttErrorKind::Protocol))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(certificates, key)
            .map_err(|_| MqttError::new("configure gateway TLS", MqttErrorKind::InvalidInput))?;
        Ok(Self {
            config: Arc::new(config),
        })
    }

    pub fn client_config(&self) -> Arc<ClientConfig> {
        self.config.clone()
    }
}

#[derive(Debug)]
struct LocalGatewayVerifier {
    roots: Arc<RootCertStore>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for LocalGatewayVerifier {
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
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            certificate,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn roots_from_pem(pem: &str, operation: &'static str) -> Result<RootCertStore, MqttError> {
    let mut reader = BufReader::new(pem.as_bytes());
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MqttError::new(operation, MqttErrorKind::InvalidInput))?;
    if certificates.is_empty() {
        return Err(MqttError::new(operation, MqttErrorKind::InvalidInput));
    }
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(certificates);
    if roots.is_empty() {
        return Err(MqttError::new(operation, MqttErrorKind::InvalidInput));
    }
    Ok(roots)
}

fn certificates_from_pem(
    pem: &str,
    operation: &'static str,
) -> Result<Vec<CertificateDer<'static>>, MqttError> {
    let mut reader = BufReader::new(pem.as_bytes());
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MqttError::new(operation, MqttErrorKind::InvalidInput))?;
    if certificates.is_empty() {
        return Err(MqttError::new(operation, MqttErrorKind::InvalidInput));
    }
    Ok(certificates)
}

fn private_key_from_pem(
    pem: &str,
    operation: &'static str,
) -> Result<PrivateKeyDer<'static>, MqttError> {
    rustls_pemfile::private_key(&mut BufReader::new(pem.as_bytes()))
        .map_err(|_| MqttError::new(operation, MqttErrorKind::InvalidInput))?
        .ok_or_else(|| MqttError::new(operation, MqttErrorKind::InvalidInput))
}
