use super::*;
use futures_lite::future::{block_on, poll_once};
use mqttbytes::v5::{SubAck, SubscribeReasonCode, UnsubAck, UnsubAckReason};
use std::time::Duration;

struct PendingIo;
impl AsyncRead for PendingIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
}
impl AsyncWrite for PendingIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
    fn poll_close(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

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
