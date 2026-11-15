use super::*;
use crate::xiaomi::tests::support::sign_csr;
use rcgen::{KeyPair, PKCS_ED25519};
use sha2::Sha256;
use x509_parser::{
    certification_request::X509CertificationRequest, pem::parse_x509_pem, prelude::FromDer as _,
};

#[test]
fn identity_is_ed25519_pkcs8_and_csr_has_expected_signed_subject() {
    let first = ClientIdentity::generate("12345").unwrap();
    let second = ClientIdentity::generate("12345").unwrap();
    assert_ne!(first.virtual_did, second.virtual_did);
    let key = KeyPair::from_pem(&first.private_key_pem).unwrap();
    assert_eq!(key.algorithm(), &PKCS_ED25519);
    let reused =
        ClientIdentity::from_private_key("12345", "987654321", &first.private_key_pem).unwrap();
    assert_eq!(reused.private_key_pem, first.private_key_pem);
    assert_eq!(reused.virtual_did, "987654321");
    let (_, pem) = parse_x509_pem(reused.csr_pem.as_bytes()).unwrap();
    let (_, csr) = X509CertificationRequest::from_der(&pem.contents).unwrap();
    csr.verify_signature().unwrap();
    let subject = &csr.certification_request_info.subject;
    assert_eq!(
        subject.iter_country().next().unwrap().as_str().unwrap(),
        "CN"
    );
    assert_eq!(
        subject
            .iter_organization()
            .next()
            .unwrap()
            .as_str()
            .unwrap(),
        "Mijia Device"
    );
    assert_eq!(
        subject.iter_common_name().next().unwrap().as_str().unwrap(),
        "mips.12345.bfe54caa6d483cc3887dce9d1b8eb91408f1ea7a.2"
    );
    assert!(!format!("{first:?}").contains(&first.private_key_pem));
}

#[test]
fn certificate_must_match_identity_subject_key_and_ordered_validity() {
    let identity = ClientIdentity::generate("12345").unwrap();
    let pem = sign_csr(&identity.csr_pem, 100, 1000).unwrap();
    assert_eq!(
        validate_certificate(
            "12345",
            &identity.virtual_did,
            &identity.private_key_pem,
            &pem
        )
        .unwrap(),
        CertificateValidity {
            not_before: 100,
            not_after: 1000
        }
    );
    assert!(
        validate_certificate(
            "other",
            &identity.virtual_did,
            &identity.private_key_pem,
            &pem
        )
        .is_err()
    );
    assert!(validate_certificate("12345", "9", &identity.private_key_pem, &pem).is_err());
    let other = ClientIdentity::generate("12345").unwrap();
    assert!(
        validate_certificate("12345", &identity.virtual_did, &other.private_key_pem, &pem).is_err()
    );
    assert!(
        validate_certificate(
            "12345",
            &identity.virtual_did,
            &identity.private_key_pem,
            "private-secret"
        )
        .is_err()
    );
    let reversed = sign_csr(&identity.csr_pem, 1_000, 100).unwrap();
    assert!(
        validate_certificate(
            "12345",
            &identity.virtual_did,
            &identity.private_key_pem,
            &reversed
        )
        .is_err()
    );
    let wrong_label = pem
        .replace("BEGIN CERTIFICATE", "BEGIN CERTIFICATE REQUEST")
        .replace("END CERTIFICATE", "END CERTIFICATE REQUEST");
    assert!(
        validate_certificate(
            "12345",
            &identity.virtual_did,
            &identity.private_key_pem,
            &wrong_label
        )
        .is_err()
    );
}

#[test]
fn validity_states_cover_future_expired_and_three_day_boundary() {
    let validity = CertificateValidity {
        not_before: 100,
        not_after: 1_000_000,
    };
    assert_eq!(validity.status(99), CertificateStatus::NotYetValid);
    assert_eq!(validity.status(100), CertificateStatus::Valid);
    assert_eq!(
        validity.status(1_000_000 - 259_201),
        CertificateStatus::Valid
    );
    assert_eq!(
        validity.status(1_000_000 - 259_200),
        CertificateStatus::RenewalDue
    );
    assert_eq!(validity.status(1_000_000), CertificateStatus::Expired);
    assert!(validity.renewal_due(1_000_000));
    assert!(!validity.currently_valid(1_000_000));
}

#[test]
fn bundled_ca_pem_bytes_match_the_upstream_file_digest() {
    assert_eq!(
        hex(&Sha256::digest(XIAOMI_CA_PEM.as_bytes())),
        "8b7bf306be3632e08b0ead308249e5f2b2520dc921ad143872d5fcc7c68d6759"
    );
}
