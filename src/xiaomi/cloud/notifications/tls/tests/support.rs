use super::*;

pub(super) const HOST: &str = "cn-ha.mqtt.io.mi.com";

pub(super) struct Fixture {
    root: CertificateDer<'static>,
    pub(super) chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl Fixture {
    pub(super) fn new(not_before: i64, not_after: i64, usage: ExtendedKeyUsagePurpose) -> Self {
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

    pub(super) fn valid() -> Self {
        Self::new(-120, 3600, ExtendedKeyUsagePurpose::ServerAuth)
    }

    pub(super) fn client_config(&self) -> Arc<ClientConfig> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = rustls_platform_verifier::Verifier::new_with_extra_roots(
            [self.root.clone()],
            provider.clone(),
        )
        .unwrap();
        client_config(provider, Arc::new(verifier)).unwrap()
    }

    pub(super) fn server_config(&self, tls12: bool) -> Arc<ServerConfig> {
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

pub(super) async fn exchange(
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
