use super::super::{CentralError, MqttFrames, wire};
use bytes::BytesMut;
use mqttbytes::{
    QoS,
    v5::{Packet, Publish},
};
use serde_json::json;

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
