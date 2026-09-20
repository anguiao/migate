use rand::random;
use rcgen::{CertificateParams, DnType, KeyPair, PKCS_ED25519};
use sha1::{Digest as _, Sha1};
use std::{error::Error as StdError, fmt};
use x509_parser::{parse_x509_certificate, pem::parse_x509_pem};

const RENEWAL_MARGIN_SECONDS: i64 = 3 * 24 * 60 * 60;

/// Xiaomi Home gateway CA bundle, copied verbatim from
/// ha_xiaomi_home/custom_components/xiaomi_home/miot/const.py.
pub const XIAOMI_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBazCCAQ+gAwIBAgIEA/UKYDAMBggqhkjOPQQDAgUAMCIxEzARBgNVBAoTCk1p\n\
amlhIFJvb3QxCzAJBgNVBAYTAkNOMCAXDTE2MTEyMzAxMzk0NVoYDzIwNjYxMTEx\n\
MDEzOTQ1WjAiMRMwEQYDVQQKEwpNaWppYSBSb290MQswCQYDVQQGEwJDTjBZMBMG\n\
ByqGSM49AgEGCCqGSM49AwEHA0IABL71iwLa4//4VBqgRI+6xE23xpovqPCxtv96\n\
2VHbZij61/Ag6jmi7oZ/3Xg/3C+whglcwoUEE6KALGJ9vccV9PmjLzAtMAwGA1Ud\n\
EwQFMAMBAf8wHQYDVR0OBBYEFJa3onw5sblmM6n40QmyAGDI5sURMAwGCCqGSM49\n\
BAMCBQADSAAwRQIgchciK9h6tZmfrP8Ka6KziQ4Lv3hKfrHtAZXMHPda4IYCIQCG\n\
az93ggFcbrG9u2wixjx1HKW4DUA5NXZG0wWQTpJTbQ==\n\
-----END CERTIFICATE-----\n\
-----BEGIN CERTIFICATE-----\n\
MIIBjzCCATWgAwIBAgIBATAKBggqhkjOPQQDAjAiMRMwEQYDVQQKEwpNaWppYSBS\n\
b290MQswCQYDVQQGEwJDTjAgFw0yMjA2MDkxNDE0MThaGA8yMDcyMDUyNzE0MTQx\n\
OFowLDELMAkGA1UEBhMCQ04xHTAbBgNVBAoMFE1JT1QgQ0VOVFJBTCBHQVRFV0FZ\n\
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEdYrzbnp/0x/cZLZnuEDXTFf8mhj4\n\
CVpZPwgj9e9Ve5r3K7zvu8Jjj7JF1JjQYvEC6yhp1SzBgglnK4L8xQzdiqNQME4w\n\
HQYDVR0OBBYEFCf9+YBU7pXDs6K6CAQPRhlGJ+cuMB8GA1UdIwQYMBaAFJa3onw5\n\
sblmM6n40QmyAGDI5sURMAwGA1UdEwQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIh\n\
AKUv+c8v98vypkGMTzMwckGjjVqTef8xodsy6PhcSCq+AiA/n9mDs62hAo5zXyJy\n\
Bs1s7mqXPf1XgieoxIvs1MqyiA==\n\
-----END CERTIFICATE-----\n";

pub const XIAOMI_CA_SHA256: &str =
    "8b7bf306be3632e08b0ead308249e5f2b2520dc921ad143872d5fcc7c68d6759";

pub struct ClientIdentity {
    pub virtual_did: String,
    pub private_key_pem: String,
    pub csr_pem: String,
}

impl ClientIdentity {
    pub fn generate(uid: &str) -> Result<Self, CertificateError> {
        let virtual_did = random::<u64>().to_string();
        let key = KeyPair::generate_for(&PKCS_ED25519).map_err(|_| CertificateError)?;
        let private_key_pem = key.serialize_pem();
        Self::from_key_pair(uid, virtual_did, private_key_pem, key)
    }

    pub fn from_private_key(
        uid: &str,
        virtual_did: &str,
        private_key_pem: &str,
    ) -> Result<Self, CertificateError> {
        if virtual_did.is_empty() || !virtual_did.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(CertificateError);
        }
        let key = KeyPair::from_pem(private_key_pem).map_err(|_| CertificateError)?;
        if key.algorithm() != &PKCS_ED25519 {
            return Err(CertificateError);
        }
        Self::from_key_pair(uid, virtual_did.to_owned(), private_key_pem.to_owned(), key)
    }

    fn from_key_pair(
        uid: &str,
        virtual_did: String,
        private_key_pem: String,
        key: KeyPair,
    ) -> Result<Self, CertificateError> {
        if uid.is_empty() {
            return Err(CertificateError);
        }
        let mut params =
            CertificateParams::new(Vec::<String>::new()).map_err(|_| CertificateError)?;
        params.distinguished_name.push(DnType::CountryName, "CN");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Mijia Device");
        params
            .distinguished_name
            .push(DnType::CommonName, expected_common_name(uid, &virtual_did));
        let csr_pem = params
            .serialize_request(&key)
            .and_then(|csr| csr.pem())
            .map_err(|_| CertificateError)?;
        Ok(Self {
            virtual_did,
            private_key_pem,
            csr_pem,
        })
    }
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientIdentity")
            .field("virtual_did", &self.virtual_did)
            .field("private_key_pem", &"[REDACTED]")
            .field("csr_pem", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CertificateValidity {
    pub not_before: i64,
    pub not_after: i64,
}

impl CertificateValidity {
    pub fn status(&self, now: i64) -> CertificateStatus {
        if now < self.not_before {
            CertificateStatus::NotYetValid
        } else if now >= self.not_after {
            CertificateStatus::Expired
        } else if self.renewal_due(now) {
            CertificateStatus::RenewalDue
        } else {
            CertificateStatus::Valid
        }
    }

    pub fn currently_valid(&self, now: i64) -> bool {
        now >= self.not_before && now < self.not_after
    }

    pub fn renewal_due(&self, now: i64) -> bool {
        now >= self.not_after.saturating_sub(RENEWAL_MARGIN_SECONDS)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertificateStatus {
    NotYetValid,
    Valid,
    RenewalDue,
    Expired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CertificateError;

impl fmt::Display for CertificateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Invalid Xiaomi gateway certificate or client identity")
    }
}

impl StdError for CertificateError {}

pub fn validate_certificate(
    uid: &str,
    virtual_did: &str,
    private_key_pem: &str,
    certificate_pem: &str,
) -> Result<CertificateValidity, CertificateError> {
    let key = KeyPair::from_pem(private_key_pem).map_err(|_| CertificateError)?;
    if key.algorithm() != &PKCS_ED25519 {
        return Err(CertificateError);
    }
    let (remaining_pem, pem) =
        parse_x509_pem(certificate_pem.as_bytes()).map_err(|_| CertificateError)?;
    if pem.label != "CERTIFICATE" || !remaining_pem.iter().all(u8::is_ascii_whitespace) {
        return Err(CertificateError);
    }
    let (remaining_der, certificate) =
        parse_x509_certificate(&pem.contents).map_err(|_| CertificateError)?;
    if !remaining_der.is_empty()
        || certificate.public_key().algorithm.algorithm.to_id_string() != "1.3.101.112"
    {
        return Err(CertificateError);
    }
    require_unique_name(certificate.subject().iter_country(), "CN")?;
    require_unique_name(certificate.subject().iter_organization(), "Mijia Device")?;
    require_unique_name(
        certificate.subject().iter_common_name(),
        &expected_common_name(uid, virtual_did),
    )?;
    if certificate.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
        return Err(CertificateError);
    }
    let not_before = certificate.validity().not_before.timestamp();
    let not_after = certificate.validity().not_after.timestamp();
    if not_after <= not_before {
        return Err(CertificateError);
    }
    Ok(CertificateValidity {
        not_before,
        not_after,
    })
}

fn require_unique_name<'a>(
    mut names: impl Iterator<Item = &'a x509_parser::x509::AttributeTypeAndValue<'a>>,
    expected: &str,
) -> Result<(), CertificateError> {
    let name = names.next().ok_or(CertificateError)?;
    if names.next().is_some() || name.as_str().ok() != Some(expected) {
        return Err(CertificateError);
    }
    Ok(())
}

fn expected_common_name(uid: &str, virtual_did: &str) -> String {
    let did_hash = Sha1::digest(virtual_did.as_bytes());
    format!("mips.{uid}.{}.2", hex(&did_hash))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests;
