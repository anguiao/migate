use super::Token;
use aes::Aes128;
use cbc::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
use md5::{Digest, Md5};

pub(super) const HELLO: [u8; 32] = [
    0x21, 0x31, 0, 32, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

pub(super) fn hello(packet: &[u8]) -> Option<(u64, u32)> {
    if packet.len() != 32 || packet[..4] != HELLO[..4] {
        return None;
    }
    let did = u64::from_be_bytes(packet[4..12].try_into().ok()?);
    let stamp = u32::from_be_bytes(packet[12..16].try_into().ok()?);
    (did != 0 && did != u64::MAX && stamp != u32::MAX).then_some((did, stamp))
}

fn key_iv(token: &Token) -> ([u8; 16], [u8; 16]) {
    let key: [u8; 16] = Md5::digest(token.bytes()).into();
    let mut digest = Md5::new();
    digest.update(key);
    digest.update(token.bytes());
    (key, digest.finalize().into())
}

pub(super) fn encode(did: u64, stamp: u32, token: &Token, payload: &[u8]) -> Vec<u8> {
    let (key, iv) = key_iv(token);
    let mut buffer = vec![0; payload.len() + 16];
    buffer[..payload.len()].copy_from_slice(payload);
    let data = cbc::Encryptor::<Aes128>::new(&key.into(), &iv.into())
        .encrypt_padded::<Pkcs7>(&mut buffer, payload.len())
        .expect("buffer has room for padding");
    let length = u16::try_from(32 + data.len()).expect("miIO payload exceeds datagram limit");
    let mut packet = Vec::with_capacity(length.into());
    packet.extend_from_slice(&[0x21, 0x31]);
    packet.extend_from_slice(&length.to_be_bytes());
    packet.extend_from_slice(&did.to_be_bytes());
    packet.extend_from_slice(&stamp.to_be_bytes());
    packet.extend_from_slice(token.bytes());
    packet.extend_from_slice(data);
    let checksum = Md5::digest(&packet);
    packet[16..32].copy_from_slice(&checksum);
    packet
}

pub(super) fn decode(packet: &[u8], token: &Token) -> Option<(u64, u32, Vec<u8>)> {
    if packet.len() < 48 || packet[..2] != [0x21, 0x31] {
        return None;
    }
    let length = u16::from_be_bytes(packet[2..4].try_into().ok()?) as usize;
    if length != packet.len() || !(length - 32).is_multiple_of(16) {
        return None;
    }
    let mut checksum = Md5::new();
    checksum.update(&packet[..16]);
    checksum.update(token.bytes());
    checksum.update(&packet[32..]);
    if checksum.finalize().as_slice() != &packet[16..32] {
        return None;
    }
    let did = u64::from_be_bytes(packet[4..12].try_into().ok()?);
    let stamp = u32::from_be_bytes(packet[12..16].try_into().ok()?);
    let (key, iv) = key_iv(token);
    let mut data = packet[32..].to_vec();
    let payload = cbc::Decryptor::<Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded::<Pkcs7>(&mut data)
        .ok()?;
    Some((did, stamp, payload.to_vec()))
}
