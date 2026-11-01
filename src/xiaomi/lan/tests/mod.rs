use super::{Token, wire};

#[test]
fn token_is_exactly_16_hex_bytes_and_debug_is_redacted() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    assert_eq!(format!("{token:?}"), "Token([REDACTED])");
    assert!(Token::parse("0011").is_none());
    assert!(Token::parse("00112233445566778899aabbccddeefg").is_none());
}

#[test]
fn authenticated_packets_round_trip_and_reject_tampering() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let payload = br#"{"id":1,"result":[{"code":0,"value":true}]}"#;
    let packet = wire::encode(656878122, 42, &token, payload);
    assert_eq!(&packet[..4], &[0x21, 0x31, 0, packet.len() as u8]);
    assert_eq!(
        wire::decode(&packet, &token),
        Some((656878122, 42, payload.to_vec()))
    );
    let mut tampered = packet;
    *tampered.last_mut().unwrap() ^= 1;
    assert_eq!(wire::decode(&tampered, &token), None);
}

#[test]
fn packet_matches_independent_openssl_vector() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let packet = wire::encode(
        656878122,
        42,
        &token,
        br#"{"id":1,"method":"miIO.info","params":[]}"#,
    );
    let actual = packet
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(
        actual,
        "213100500000000027272a2a0000002ad2030efa413c3c783e8287d5c8901e27a5516ec6151955dc2bb2d43e7c84c18352a39a7f22fd2903d282a5783c6d56ac8c9c7629130a25fac17d38175c1a7799"
    );
}

pub(crate) mod support;

#[test]
fn miot_datagrams_keep_the_full_device_identifier() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let did = u64::from(u32::MAX) + 123;
    let packet = wire::encode(did, 42, &token, b"{}");
    assert_eq!(wire::decode(&packet, &token).unwrap().0, did);
    let mut hello = wire::HELLO;
    hello[4..12].copy_from_slice(&did.to_be_bytes());
    hello[12..16].copy_from_slice(&42_u32.to_be_bytes());
    assert_eq!(wire::hello(&hello), Some((did, 42)));
}
