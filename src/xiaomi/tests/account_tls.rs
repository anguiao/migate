use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use std::{
    net::{SocketAddr, TcpStream},
    sync::{Arc, OnceLock},
};

pub(super) fn accept(
    socket: TcpStream,
) -> rustls::StreamOwned<rustls::ServerConnection, TcpStream> {
    let server = rustls::ServerConnection::new(account_tls().server.clone()).unwrap();
    rustls::StreamOwned::new(server, socket)
}

struct AccountTls {
    server: Arc<rustls::ServerConfig>,
    root: Vec<u8>,
}

fn account_tls() -> &'static AccountTls {
    static TLS: OnceLock<AccountTls> = OnceLock::new();
    TLS.get_or_init(|| {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "MiGate account test CA");
        let root = params.self_signed(&key).unwrap();
        let issuer = Issuer::new(params, key);
        let names = [
            "account.xiaomi.com",
            "c3.account.xiaomi.com",
            "c3.lp.account.xiaomi.com",
            "sts.api.io.mi.com",
        ];
        let leaf = CertificateParams::new(names.map(str::to_owned)).unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = leaf.signed_by(&key, &issuer).unwrap();
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap();
        AccountTls {
            server: server.into(),
            root: root.der().to_vec(),
        }
    })
}

pub(in crate::xiaomi) fn client_builder(address: SocketAddr) -> isahc::HttpClientBuilder {
    use isahc::{
        config::Configurable,
        net::Dialer,
        tls::{TlsConfig, TrustStore},
    };
    let trust = TrustStore::builder()
        .certificate_from_der(&account_tls().root)
        .build();
    isahc::HttpClient::builder()
        .dial(Dialer::ip_socket(address))
        .tls_config(TlsConfig::builder().trust_store(trust).build())
        .proxy(None)
}
