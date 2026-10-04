use super::*;
use bytes::BytesMut;
use mqttbytes::{
    QoS,
    v5::{Packet, Publish},
};
use serde_json::json;

use crate::{
    storage::{TokenSet, XiaomiRecord},
    xiaomi::certificate::ClientIdentity,
};
use async_io::Async;
use futures_lite::{
    future::{self, block_on},
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
    force_tls12: bool,
}

impl TlsFixture {
    pub(crate) fn new(expired_server: bool) -> Self {
        Self::server_case(
            -120,
            if expired_server { -60 } else { 3600 },
            ExtendedKeyUsagePurpose::ServerAuth,
        )
    }

    fn server_case(not_before: i64, not_after: i64, usage: ExtendedKeyUsagePurpose) -> Self {
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

async fn exchange_tls(
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

#[test]
fn tlv_and_mqtt_frames_preserve_fragmented_and_coalesced_messages() {
    let payload = json!({"value":true});
    let encoded = wire::encode_mips(7, Some("client/reply"), &payload);
    let message = wire::decode_mips(&encoded).unwrap();
    assert_eq!(message.id, 7);
    assert_eq!(message.payload, payload);
    let mut publish = Publish::new("client/reply", QoS::ExactlyOnce, encoded);
    publish.pkid = 2;
    let mut bytes = BytesMut::new();
    publish.write(&mut bytes).unwrap();
    let mut frames = wire::Frames::default();
    frames.push(&bytes[..3]).unwrap();
    assert!(frames.next_packet().unwrap().is_none());
    frames.push(&bytes[3..]).unwrap();
    frames.push(&[0xd0, 0]).unwrap();
    assert!(matches!(
        frames.next_packet().unwrap(),
        Some(Packet::Publish(_))
    ));
    assert!(matches!(
        frames.next_packet().unwrap(),
        Some(Packet::PingResp)
    ));
    assert!(frames.next_packet().unwrap().is_none());
}

fn manual_mips_fields(fields: &[(u8, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (tag, value) in fields {
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        bytes.push(*tag);
        bytes.extend_from_slice(value);
    }
    bytes
}

#[test]
fn mips_string_fields_accept_optional_trailing_nul() {
    for from in [b"gateway".as_slice(), b"gateway\0".as_slice()] {
        for reply_topic in [b"client/reply".as_slice(), b"client/reply\0".as_slice()] {
            for payload in [
                b"{\"result\":[]}".as_slice(),
                b"{\"result\":[]}\0".as_slice(),
            ] {
                let bytes = manual_mips_fields(&[
                    (0, &7u32.to_le_bytes()),
                    (3, from),
                    (1, reply_topic),
                    (2, payload),
                ]);
                let message = wire::decode_mips(&bytes).unwrap();
                assert_eq!(message.id, 7);
                assert_eq!(message.payload, json!({"result":[]}));
            }
        }
    }
}

#[test]
fn mips_string_fields_reject_embedded_nul_and_invalid_utf8() {
    for tag in [3, 1, 2] {
        for invalid in [
            b"bad\0value".as_slice(),
            b"bad\0value\0".as_slice(),
            b"{}\0\0".as_slice(),
            b"\xff".as_slice(),
            b"\xff\0".as_slice(),
        ] {
            let mut fields = [
                (0, [7, 0, 0, 0].as_slice()),
                (3, b"gateway".as_slice()),
                (1, b"client/reply".as_slice()),
                (2, b"{\"result\":[]}".as_slice()),
            ];
            fields.iter_mut().find(|field| field.0 == tag).unwrap().1 = invalid;
            assert_eq!(
                wire::decode_mips(&manual_mips_fields(&fields)).unwrap_err(),
                CentralError::Protocol
            );
        }
    }
}

#[test]
fn mips_payload_rejects_invalid_json() {
    for payload in [b"{\"result\":".as_slice(), b"{\"result\":\0".as_slice()] {
        let bytes = manual_mips_fields(&[(0, &7u32.to_le_bytes()), (2, payload)]);
        assert_eq!(
            wire::decode_mips(&bytes).unwrap_err(),
            CentralError::Protocol
        );
    }
}

#[test]
fn mips_rejects_truncated_field_lengths() {
    let bytes = manual_mips_fields(&[
        (0, &7u32.to_le_bytes()),
        (3, b"gateway"),
        (1, b"client/reply\0"),
        (2, b"{\"result\":[]}"),
    ]);
    for length in 0..bytes.len() {
        assert_eq!(
            wire::decode_mips(&bytes[..length]).unwrap_err(),
            CentralError::Protocol
        );
    }
}

#[test]
fn malformed_tlv_or_frame_errors_do_not_disclose_payloads() {
    for bytes in [
        vec![],
        vec![4, 0, 0, 0, 0, 1],
        vec![0, 0, 0, 0, 2],
        b"private payload".to_vec(),
    ] {
        let error = wire::decode_mips(&bytes).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("private payload"));
    }
    let mut bytes = wire::encode_mips(1, None, &json!({"value":true}));
    bytes.extend_from_slice(&[4, 0, 0, 0, 0, 1, 0, 0, 0]);
    assert!(wire::decode_mips(&bytes).is_err());
    let mut frames = wire::Frames::default();
    frames.push(&[0xff, 0]).unwrap();
    assert_eq!(frames.next_packet().unwrap_err(), CentralError::Protocol);
}

#[test]
fn mqtt_frames_accept_short_disconnect_and_reject_invalid_flags_and_sizes() {
    let mut frames = MqttFrames::default();
    frames.push(&[0xe0, 0]).unwrap();
    assert!(matches!(
        frames.next_packet().unwrap(),
        Some(Packet::Disconnect(_))
    ));
    for bytes in [
        vec![0xd1, 0],
        vec![0xc0, 1, 0],
        vec![0xe0, 1, 0xff],
        vec![0x30, 0xff, 0xff, 0xff, 0x7f],
        vec![0x30, 0xff, 0xff, 0xff, 0xff, 0],
    ] {
        let mut frames = MqttFrames::default();
        frames.push(&bytes).unwrap();
        assert_eq!(frames.next_packet().unwrap_err(), CentralError::Protocol);
    }
}

#[test]
fn complete_frames_with_missing_mandatory_fields_are_errors_and_reason_only_packets_are_valid() {
    for bytes in [
        vec![0x20, 2, 0, 0],
        vec![0x30, 2, 0, 0],
        vec![0x90, 2, 0, 1],
    ] {
        let mut frames = MqttFrames::default();
        frames.push(&bytes).unwrap();
        assert_eq!(frames.next_packet().unwrap_err(), CentralError::Protocol);
    }
    for (bytes, reason) in [
        (vec![0xe0, 1, 0x87], 0x87),
        (vec![0x40, 3, 0, 1, 0x87], 0x87),
        (vec![0x50, 3, 0, 1, 0x87], 0x87),
        (vec![0x62, 3, 0, 1, 0x92], 0x92),
        (vec![0x70, 3, 0, 1, 0x92], 0x92),
    ] {
        let mut frames = MqttFrames::default();
        frames.push(&bytes[..2]).unwrap();
        assert!(frames.next_packet().unwrap().is_none());
        frames.push(&bytes[2..]).unwrap();
        frames.push(&[0xd0, 0]).unwrap();
        let actual = match frames.next_packet().unwrap().unwrap() {
            Packet::Disconnect(packet) => packet.reason_code as u8,
            Packet::PubAck(packet) => packet.reason as u8,
            Packet::PubRec(packet) => packet.reason as u8,
            Packet::PubRel(packet) => packet.reason as u8,
            Packet::PubComp(packet) => packet.reason as u8,
            other => panic!("unexpected packet {other:?}"),
        };
        assert_eq!(actual, reason);
        assert!(matches!(
            frames.next_packet().unwrap(),
            Some(Packet::PingResp)
        ));
        assert!(frames.next_packet().unwrap().is_none());
    }
}
