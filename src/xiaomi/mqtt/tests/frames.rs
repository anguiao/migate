use super::*;
use bytes::BytesMut;
use mqttbytes::{
    QoS,
    v5::{ConnAck, ConnectReturnCode, Packet, Publish},
};

#[test]
fn connack_roundtrips_through_encoding_and_framing() {
    let bytes = encode_packet(&Packet::ConnAck(ConnAck::new(
        ConnectReturnCode::Success,
        false,
    )))
    .unwrap();
    let mut frames = Frames::default();
    frames.push(&bytes).unwrap();
    assert!(matches!(
        frames.next_packet().unwrap(),
        Some(Packet::ConnAck(_))
    ));
}

#[test]
fn partial_frames_stay_pending_until_complete() {
    // A packet id is only encoded for a QoS exchange.
    let mut packet = Publish::new("topic", mqttbytes::QoS::AtLeastOnce, "payload".to_string());
    packet.pkid = 9;
    let bytes = encode_packet(&Packet::Publish(packet)).unwrap();
    let split = bytes.len() / 2;
    let mut frames = Frames::default();
    frames.push(&bytes[..split]).unwrap();
    assert!(frames.next_packet().unwrap().is_none());
    frames.push(&bytes[split..]).unwrap();
    let Some(Packet::Publish(publish)) = frames.next_packet().unwrap() else {
        panic!("complete PUBLISH expected");
    };
    assert_eq!(publish.topic, "topic");
    assert_eq!(publish.pkid, 9);
}

#[test]
fn oversized_receive_buffer_is_rejected() {
    let mut frames = Frames::default();
    assert!(
        frames.push(&vec![0; MAX_FRAME_SIZE + 4096]).is_ok(),
        "the limit allows a full frame plus slack"
    );
    assert!(frames.push(&[0]).is_err());
}

#[test]
fn invalid_fixed_header_and_nonempty_ping_are_rejected() {
    let mut frames = Frames::default();
    frames.push(&[0x00, 0x00]).unwrap();
    assert_eq!(frames.next_packet(), Err(ProtocolError));

    let mut frames = Frames::default();
    frames.push(&[0xc0, 0x01, 0x00]).unwrap();
    assert_eq!(frames.next_packet(), Err(ProtocolError));
}

#[test]
fn disconnect_with_omitted_reason_decodes() {
    let mut frames = Frames::default();
    frames.push(&[0xe0, 0x00]).unwrap();
    assert!(matches!(
        frames.next_packet().unwrap(),
        Some(Packet::Disconnect(_))
    ));
}

#[test]
fn packet_context_reports_type_and_packet_id() {
    assert_eq!(packet_context(&Packet::PingReq), ("PINGREQ", None));
    let mut publish = Publish::new("topic", QoS::ExactlyOnce, "payload".to_string());
    publish.pkid = 7;
    assert_eq!(
        packet_context(&Packet::Publish(publish)),
        ("PUBLISH", Some(7))
    );
}

#[test]
fn fragmented_and_coalesced_frames_decode_in_order() {
    let mut publish = Publish::new("client/reply", QoS::ExactlyOnce, "payload".to_string());
    publish.pkid = 2;
    let mut bytes = BytesMut::new();
    publish.write(&mut bytes).unwrap();
    let mut frames = Frames::default();
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
fn invalid_flags_sizes_and_missing_mandatory_fields_are_rejected() {
    for bytes in [
        vec![0xff, 0],
        vec![0xd1, 0],
        vec![0xe0, 1, 0xff],
        vec![0x30, 0xff, 0xff, 0xff, 0x7f],
        vec![0x30, 0xff, 0xff, 0xff, 0xff, 0],
        vec![0x20, 2, 0, 0],
        vec![0x30, 2, 0, 0],
        vec![0x90, 2, 0, 1],
    ] {
        let mut frames = Frames::default();
        frames.push(&bytes).unwrap();
        assert_eq!(frames.next_packet(), Err(ProtocolError), "bytes: {bytes:?}");
    }
}

#[test]
fn reason_only_packets_decode_segmented_and_coalesced() {
    for (bytes, reason) in [
        (vec![0xe0, 1, 0x87], 0x87),
        (vec![0x40, 3, 0, 1, 0x87], 0x87),
        (vec![0x50, 3, 0, 1, 0x87], 0x87),
        (vec![0x62, 3, 0, 1, 0x92], 0x92),
        (vec![0x70, 3, 0, 1, 0x92], 0x92),
    ] {
        let mut frames = Frames::default();
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
