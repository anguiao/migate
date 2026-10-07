use super::*;

#[test]
fn cancelled_and_expired_subscriptions_reserve_packet_ids_until_their_late_ack() {
    block_on(async {
        for case in ["cancelled", "expired", "unsubscribe"] {
            let (handle, mut driver) = SessionDriver::new(
                Box::new(PendingIo),
                wire::Frames::default(),
                "client",
                Timing::default(),
                wire::MAX_FRAME_SIZE,
                LIMIT,
            )
            .unwrap();
            driver.subscriptions.clear();
            driver.output.clear();
            driver.ready = true;
            let subscribe =
                handle.subscribe_property("target", PropertyBinding { siid: 2, piid: 1 });
            driver.drain_requests().unwrap();
            let mut old_id = *driver.subscriptions.keys().next().unwrap();
            // Treat this frame as sent; its acknowledgement can still arrive.
            driver.output.clear();
            let mut unsubscribe = None;
            if case == "cancelled" {
                handle.cancel_device("target").unwrap();
                driver.drain_cancellations();
                assert_eq!(subscribe.await, Err(CentralError::Cancelled));
            } else if case == "unsubscribe" {
                driver
                    .packet(
                        Packet::SubAck(SubAck::new(old_id, vec![SubscribeReasonCode::QoS2])),
                        &mut |_| {},
                    )
                    .unwrap();
                subscribe.await.unwrap();
                unsubscribe = Some(handle.unsubscribe_device("target"));
                driver.drain_cancellations();
                driver.drain_requests().unwrap();
                old_id = *driver.subscriptions.keys().next().unwrap();
                driver.output.clear();
            } else {
                drop(subscribe);
            }
            let old = driver.subscriptions.get_mut(&old_id).unwrap();
            old.deadline = Instant::now() - Duration::from_secs(1);
            if let Some(request) = &mut old.request {
                request.deadline = old.deadline;
            }
            driver.expire().unwrap();
            assert!(
                driver.subscriptions.contains_key(&old_id),
                "{case}: packet id released before acknowledgement"
            );
            assert!(driver.next_deadline() > Instant::now());
            if case == "expired" {
                handle.cancel_device("target").unwrap();
                driver.drain_cancellations();
                assert!(
                    driver.retired_topics["target"]
                        .contains("master/appMsg/notify/iot/target/property/2.1")
                );
            }
            if let Some(unsubscribe) = unsubscribe {
                assert_eq!(unsubscribe.await, Err(CentralError::Timeout));
            }
            // Force the allocator to revisit the old id without 65,535 requests.
            driver.pkid = old_id.wrapping_sub(1);
            let mut rejoined =
                Box::pin(handle.subscribe_property("target", PropertyBinding { siid: 2, piid: 1 }));
            driver.drain_requests().unwrap();
            let new_id = *driver
                .subscriptions
                .keys()
                .find(|id| **id != old_id)
                .unwrap();
            assert_ne!(new_id, old_id);
            let late = if case == "unsubscribe" {
                let mut ack = UnsubAck::new(old_id);
                ack.reasons = vec![UnsubAckReason::Success];
                Packet::UnsubAck(ack)
            } else {
                Packet::SubAck(SubAck::new(old_id, vec![SubscribeReasonCode::QoS2]))
            };
            driver.packet(late, &mut |_| {}).unwrap();
            assert!(poll_once(rejoined.as_mut()).await.is_none());
            assert!(driver.property_topics.is_empty());
            driver
                .packet(
                    Packet::SubAck(SubAck::new(new_id, vec![SubscribeReasonCode::QoS2])),
                    &mut |_| {},
                )
                .unwrap();
            rejoined.await.unwrap();
            assert_eq!(driver.property_topics.len(), 1);
        }
    });
}

#[test]
fn refresh_timeout_keeps_the_ten_second_qos2_handshake_deadline() {
    block_on(async {
        let (handle, mut driver) = SessionDriver::new(
            Box::new(PendingIo),
            wire::Frames::default(),
            "client",
            Timing::default(),
            wire::MAX_FRAME_SIZE,
            LIMIT,
        )
        .unwrap();
        driver.subscriptions.clear();
        driver.output.clear();
        driver.ready = true;
        let before = Instant::now();
        let read = handle.read_property_with_timeout(
            "target",
            PropertyBinding { siid: 2, piid: 1 },
            Duration::from_secs(6),
            |result| assert_eq!(result, Err(CentralError::Timeout)),
        );
        driver.drain_requests().unwrap();
        let after = Instant::now();
        let (&pkid, publication) = driver.publications.iter().next().unwrap();
        assert!(publication.deadline >= before + Duration::from_secs(10));
        assert!(publication.deadline <= after + Duration::from_secs(10));
        let mid = publication.mid;
        let request = driver.pending.get_mut(&mid).unwrap();
        assert!(request.deadline >= before + Duration::from_secs(6));
        assert!(request.deadline <= after + Duration::from_secs(6));
        request.deadline = Instant::now() - Duration::from_secs(1);
        // The PUBLISH has left the client, but its QoS2 handshake is unfinished.
        driver.output.clear();
        driver.expire().unwrap();
        assert_eq!(read.await, Err(CentralError::Timeout));
        assert!(driver.publications.contains_key(&pkid));
        driver
            .packet(
                Packet::PubRec(mqttbytes::v5::PubRec::new(pkid)),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            driver.publications[&pkid].stage,
            PublicationStage::AwaitPubComp
        );
        assert!(driver.output[0].deadline >= before + Duration::from_secs(10));
        driver
            .packet(
                Packet::PubComp(mqttbytes::v5::PubComp::new(pkid)),
                &mut |_| {},
            )
            .unwrap();
        assert!(driver.publications.is_empty());
    });
}
