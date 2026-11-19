//! DNS-SD assertions and the isolated operational fabric for process tests.

use super::process_support::TIMEOUT;
use crate::storage::Store;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::time::Instant;

pub(super) const COMMISSIONABLE_SERVICE: &str = "_matterc._udp.local.";
pub(super) const OPERATIONAL_SERVICE: &str = "_matter._tcp.local.";

pub(super) fn commissionable_name(logs: &str) -> String {
    let (_, service) = logs
        .split_once("Registering mDNS service: Commissionable { id: ")
        .unwrap();
    let id: u64 = service.split_once(',').unwrap().0.parse().unwrap();
    format!("{id:016X}")
}

pub(super) fn assert_resolves(instance: &str, service_type: &str, port: u16) {
    let daemon = ServiceDaemon::new().unwrap();
    let events = daemon.browse(service_type).unwrap();
    let fullname = format!("{instance}.{service_type}");
    let deadline = Instant::now() + TIMEOUT;
    let mut resolved_port = None;
    while let Ok(event) = events.recv_deadline(deadline) {
        if let ServiceEvent::ServiceResolved(service) = event
            && service.get_fullname().eq_ignore_ascii_case(&fullname)
        {
            resolved_port = Some(service.get_port());
            break;
        }
    }
    daemon.shutdown().unwrap().recv_timeout(TIMEOUT).unwrap();
    assert_eq!(resolved_port, Some(port), "resolving {fullname}");
}

/// Persist a fresh fabric and return its operational instance name.
pub(super) fn store_test_fabric(store: &Store) -> String {
    use rs_matter::{
        cert::{MAX_CERT_TLV_AND_ASN1_LEN, r#gen::VALID_FOREVER},
        crypto::{
            CanonAeadKey, CanonPkcSecretKey, Crypto, Rng, SecretKey, SigningSecretKey,
            default_crypto,
        },
        dm::devices::test::DAC_PRIVKEY,
        fabric::Fabrics,
        onboard::{cac::RcacGenerator, noc::NocGenerator},
        persist::{FABRIC_KEYS_START, KV_BUF_SIZE},
        tlv::{TLVTag, ToTLV},
        utils::storage::WriteBuf,
    };

    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let mut rcac_buffer = [0; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut rcac_generator = RcacGenerator::new(&mut rcac_buffer);
    let (rcac_key, rcac) = rcac_generator.generate(&crypto, 1, VALID_FOREVER).unwrap();
    let key = crypto.generate_secret_key().unwrap();
    let mut private_key = CanonPkcSecretKey::new();
    key.write_canon(&mut private_key).unwrap();
    let mut csr_buffer = [0; 256];
    let csr = key.csr(&mut csr_buffer).unwrap();
    let mut noc_buffer = [0; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut noc_generator =
        NocGenerator::create(rcac_key.reference(), rcac, &[], &mut noc_buffer).unwrap();
    let noc = noc_generator
        .generate(&crypto, csr, 2, &[], VALID_FOREVER)
        .unwrap();
    let mut ipk = CanonAeadKey::new();
    crypto.rand().unwrap().fill_bytes(ipk.access_mut());
    let mut fabrics = Fabrics::new();
    let fabric = fabrics
        .add(
            &crypto,
            private_key.reference(),
            rcac,
            noc,
            &[],
            Some(ipk.reference()),
            0xFFF1,
            3,
        )
        .unwrap();
    let mut buffer = [0; KV_BUF_SIZE];
    let mut writer = WriteBuf::new(&mut buffer);
    fabric.to_tlv(&TLVTag::Anonymous, &mut writer).unwrap();
    store
        .matter()
        .put(
            FABRIC_KEYS_START + u16::from(fabric.fab_idx().get()),
            writer.as_slice(),
        )
        .unwrap();
    format!(
        "{:016X}-{:016X}",
        fabric.compressed_fabric_id(),
        fabric.node_id()
    )
}
