use aes::{
    Aes128,
    cipher::{BlockModeDecrypt as _, BlockModeEncrypt as _, KeyIvInit as _, block_padding::Pkcs7},
};
use md5::{Digest as _, Md5};
use serde_json::Value;
use subtle::ConstantTimeEq as _;

use super::{LanError, LanErrorKind};

const HEADER_LENGTH: usize = 32;
pub(super) const MAX_PACKET_LENGTH: usize = 1400;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct DecodedPacket {
    pub did: u64,
    pub timestamp: u32,
    pub message: Value,
}

pub(super) fn encode_packet(
    did: u64,
    timestamp: u32,
    token: &[u8; 16],
    plaintext: &[u8],
) -> Result<Vec<u8>, LanError> {
    if did == 0 || plaintext.is_empty() || plaintext.len() + HEADER_LENGTH + 16 > MAX_PACKET_LENGTH
    {
        return Err(LanError::new(
            "encode LAN packet",
            LanErrorKind::InvalidInput,
        ));
    }
    let (key, iv) = key_iv(token);
    let mut encrypted = vec![0_u8; plaintext.len() + 16];
    encrypted[..plaintext.len()].copy_from_slice(plaintext);
    let encrypted = cbc::Encryptor::<Aes128>::new(&key.into(), &iv.into())
        .encrypt_padded::<Pkcs7>(&mut encrypted, plaintext.len())
        .map_err(|_| LanError::new("encode LAN packet", LanErrorKind::Protocol))?;
    let packet_length = HEADER_LENGTH + encrypted.len();
    let packet_length_u16 = u16::try_from(packet_length)
        .map_err(|_| LanError::new("encode LAN packet", LanErrorKind::InvalidInput))?;
    let mut packet = Vec::with_capacity(packet_length);
    packet.extend_from_slice(&[0x21, 0x31]);
    packet.extend_from_slice(&packet_length_u16.to_be_bytes());
    packet.extend_from_slice(&did.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(token);
    packet.extend_from_slice(encrypted);
    let checksum = Md5::digest(&packet);
    packet[16..32].copy_from_slice(&checksum);
    Ok(packet)
}

pub(super) fn decode_packet(
    packet: &[u8],
    expected_did: u64,
    token: &[u8; 16],
) -> Result<DecodedPacket, LanError> {
    if packet.len() <= HEADER_LENGTH
        || packet.len() > MAX_PACKET_LENGTH
        || packet[..2] != [0x21, 0x31]
        || usize::from(u16::from_be_bytes([packet[2], packet[3]])) != packet.len()
        || !(packet.len() - HEADER_LENGTH).is_multiple_of(16)
    {
        return Err(LanError::new("decode LAN packet", LanErrorKind::Protocol));
    }
    let mut did_bytes = [0_u8; 8];
    did_bytes.copy_from_slice(&packet[4..12]);
    let did = u64::from_be_bytes(did_bytes);
    if did != expected_did {
        return Err(LanError::new("decode LAN packet", LanErrorKind::Protocol));
    }
    let mut timestamp_bytes = [0_u8; 4];
    timestamp_bytes.copy_from_slice(&packet[12..16]);
    let timestamp = u32::from_be_bytes(timestamp_bytes);
    let received_checksum = &packet[16..32];
    let mut authenticated = packet.to_vec();
    authenticated[16..32].copy_from_slice(token);
    let expected_checksum = Md5::digest(&authenticated);
    if !bool::from(received_checksum.ct_eq(expected_checksum.as_slice())) {
        return Err(LanError::new("decode LAN packet", LanErrorKind::Protocol));
    }
    let (key, iv) = key_iv(token);
    let mut plaintext = packet[HEADER_LENGTH..].to_vec();
    let plaintext = cbc::Decryptor::<Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded::<Pkcs7>(&mut plaintext)
        .map_err(|_| LanError::new("decode LAN packet", LanErrorKind::Protocol))?;
    let mut end = plaintext.len();
    while end > 0 && plaintext[end - 1] == 0 {
        end -= 1;
    }
    let message = serde_json::from_slice::<Value>(&plaintext[..end])
        .map_err(|_| LanError::new("decode LAN packet", LanErrorKind::Protocol))?;
    if !message.is_object() {
        return Err(LanError::new("decode LAN packet", LanErrorKind::Protocol));
    }
    Ok(DecodedPacket {
        did,
        timestamp,
        message,
    })
}

fn key_iv(token: &[u8; 16]) -> ([u8; 16], [u8; 16]) {
    let key: [u8; 16] = Md5::digest(token).into();
    let mut iv_input = [0_u8; 32];
    iv_input[..16].copy_from_slice(&key);
    iv_input[16..].copy_from_slice(token);
    let iv: [u8; 16] = Md5::digest(iv_input).into();
    (key, iv)
}

pub(super) fn native_probe(virtual_did: u64) -> [u8; 32] {
    let mut packet = [0xff; 32];
    packet[..4].copy_from_slice(&[0x21, 0x31, 0x00, 0x20]);
    packet[16..20].copy_from_slice(b"MDID");
    packet[20..28].copy_from_slice(&virtual_did.to_be_bytes());
    packet[28..].fill(0);
    packet
}

pub(super) const LEGACY_PROBE: [u8; 32] = [
    0x21, 0x31, 0x00, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];
