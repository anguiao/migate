use super::*;
use mqttbytes::QoS;
use mqttbytes::v5::{PubRelReason, Publish, PublishProperties};

fn publish(pkid: u16, payload: &str) -> Publish {
    let mut publish = Publish::new("topic", QoS::ExactlyOnce, payload.to_string());
    publish.pkid = pkid;
    publish
}

fn duplicate(pkid: u16, payload: &str) -> Publish {
    let mut duplicate = publish(pkid, payload);
    duplicate.dup = true;
    duplicate
}

#[test]
fn duplicates_keep_the_first_context_and_require_identical_content() {
    let mut qos2 = QoS2::new(4);
    qos2.receive(&publish(7, "one"), || "first").unwrap();
    qos2.receive(&duplicate(7, "one"), || "second").unwrap();
    let received = qos2
        .release(7, PubRelReason::Success)
        .unwrap()
        .expect("first capture");
    assert_eq!(received.context, "first");
    assert_eq!(&received.publish.payload[..], b"one");
    assert_eq!(qos2.len(), 0);
}

#[test]
fn conflicting_or_unflagged_duplicates_are_rejected() {
    let mut qos2 = QoS2::new(4);
    qos2.receive(&publish(7, "one"), || "first").unwrap();

    assert!(
        qos2.receive(&duplicate(7, "two"), || "second").is_err(),
        "different content under the same packet id"
    );
    assert!(
        qos2.receive(&publish(7, "one"), || "second").is_err(),
        "a repeat must be flagged as a duplicate"
    );
    assert!(qos2.receive(&duplicate(7, "one"), || "second").is_ok());
}

#[test]
fn capacity_and_zero_packet_ids_are_rejected() {
    let mut qos2 = QoS2::new(1);
    qos2.receive(&publish(1, "one"), || ()).unwrap();
    assert!(qos2.receive(&publish(2, "two"), || ()).is_err());
    assert!(qos2.receive(&publish(0, "zero"), || ()).is_err());
    assert_eq!(qos2.len(), 1);
}

#[test]
fn release_validates_the_handshake_before_removing_a_capture() {
    let mut qos2 = QoS2::new(4);
    qos2.receive(&publish(7, "one"), || 3).unwrap();
    assert!(matches!(
        qos2.release(0, PubRelReason::Success),
        Err(ProtocolError)
    ));
    assert!(matches!(
        qos2.release(7, PubRelReason::PacketIdentifierNotFound),
        Err(ProtocolError)
    ));
    assert_eq!(qos2.len(), 1, "an invalid PUBREL must not drop the capture");
    assert!(qos2.release(9, PubRelReason::Success).unwrap().is_none());
    assert_eq!(
        qos2.release(7, PubRelReason::Success)
            .unwrap()
            .unwrap()
            .context,
        3
    );
    assert!(qos2.release(7, PubRelReason::Success).unwrap().is_none());
}

#[test]
fn empty_topics_and_topic_aliases_are_invalid_publishes() {
    let mut empty = publish(1, "value");
    empty.topic.clear();
    assert!(!valid_publish(&empty));

    let mut aliased = publish(1, "value");
    aliased.properties = Some(PublishProperties {
        payload_format_indicator: None,
        message_expiry_interval: None,
        topic_alias: Some(1),
        response_topic: None,
        correlation_data: None,
        user_properties: Vec::new(),
        subscription_identifiers: Vec::new(),
        content_type: None,
    });
    assert!(!valid_publish(&aliased));
    assert!(valid_publish(&publish(1, "value")));
}
