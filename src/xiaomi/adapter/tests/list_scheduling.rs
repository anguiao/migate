use super::*;
use crate::xiaomi::{
    adapter::state::{AccessRefresh, AccessRequest, Backoff},
    central::{DeviceAccess, DeviceEntry},
};

#[test]
fn initial_list_is_immediate_and_later_requests_keep_the_three_second_deadline() {
    block_on(async {
        let clock = TestClock::default();
        let access = AccessRefresh::default();
        clock.advance(Duration::from_secs(30));
        access.request_initial(clock.now());
        access.request(clock.now());
        assert!(matches!(
            poll_once(access.next(&clock)).await,
            Some(AccessRequest::Full)
        ));
        assert!(poll_once(access.next(&clock)).await.is_none());
        for _ in 0..3 {
            access.request(clock.now());
            assert!(poll_once(access.next(&clock)).await.is_none());
            clock.advance(Duration::from_secs(2));
            access.request(clock.now());
            assert!(poll_once(access.next(&clock)).await.is_none());
            clock.advance(Duration::from_millis(999));
            assert!(poll_once(access.next(&clock)).await.is_none());
            clock.advance(Duration::from_millis(1));
            assert!(matches!(access.next(&clock).await, AccessRequest::Full));
            assert!(poll_once(access.next(&clock)).await.is_none());
        }
    });
}

#[test]
fn full_list_failure_restarts_the_three_second_delay_without_blocking_filtered_changes() {
    block_on(async {
        let clock = TestClock::default();
        let access = AccessRefresh::default();
        access.request_initial(clock.now());
        assert!(matches!(access.next(&clock).await, AccessRequest::Full));
        clock.advance(Duration::from_secs(1));
        access.request(clock.now());
        clock.advance(Duration::from_secs(9));
        access.retry_full(clock.now());
        access.request_filtered(vec!["one".into()]);
        let AccessRequest::Filtered(dids) = access.next(&clock).await else {
            panic!("filtered changes must remain independent of a failed full list")
        };
        assert_eq!(dids, ["one"]);
        assert!(poll_once(access.next(&clock)).await.is_none());
        clock.advance(Duration::from_millis(2999));
        assert!(poll_once(access.next(&clock)).await.is_none());
        clock.advance(Duration::from_millis(1));
        assert!(matches!(access.next(&clock).await, AccessRequest::Full));
    });
}

#[test]
fn changed_dids_merge_without_delaying_the_pending_full_list() {
    block_on(async {
        let clock = TestClock::default();
        let access = AccessRefresh::default();
        access.request_initial(clock.now());
        assert!(matches!(access.next(&clock).await, AccessRequest::Full));
        access.request(clock.now());
        clock.advance(Duration::from_secs(1));
        access.request_filtered(vec!["one".into(), "multi".into()]);
        access.request_filtered(vec!["one".into()]);
        let AccessRequest::Filtered(dids) = access.next(&clock).await else {
            panic!("changes must be queried before the full list deadline")
        };
        assert_eq!(dids, ["multi", "one"]);
        assert!(poll_once(access.next(&clock)).await.is_none());

        // Changes arriving after dispatch belong to the next filtered request.
        access.request_filtered(vec!["two".into()]);
        let AccessRequest::Filtered(dids) = access.next(&clock).await else {
            panic!("new changes must survive an in-flight filtered request")
        };
        assert_eq!(dids, ["two"]);
        access.request_filtered(vec!["one".into()]);
        clock.advance(Duration::from_secs(2));
        assert!(matches!(access.next(&clock).await, AccessRequest::Full));
        assert!(poll_once(access.next(&clock)).await.is_none());
    });
}

#[test]
fn a_new_local_session_requests_its_first_list_without_advancing_the_clock() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            until(|| peer.lists() == 1).await;
            assert_eq!(control.clock.now(), Duration::ZERO);
        })
        .await;
    }));
}

#[test]
fn initial_list_waits_for_baseline_suback_and_preserves_its_timeout() {
    for acknowledge in [true, false] {
        let rig = Rig::new(true, &["one"]);
        rig.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        block_on(rig.run(|c| async move {
            c.announce();
            let mut broker = c.accept().await;
            let Packet::Connect(connect) = broker.next().await else {
                panic!("CONNECT expected")
            };
            broker.client_id = connect.client_id;
            broker
                .send(Packet::ConnAck(ConnAck::new(
                    ConnectReturnCode::Success,
                    false,
                )))
                .await;
            let Packet::Subscribe(subscription) = broker.next().await else {
                panic!("baseline SUBSCRIBE expected")
            };
            c.clock.advance(Duration::from_secs(4));
            future::or(
                async {
                    panic!(
                        "business packet before baseline SUBACK: {:?}",
                        broker.next().await
                    )
                },
                async {
                    async_io::Timer::after(Duration::from_millis(40)).await;
                },
            )
            .await;
            if acknowledge {
                broker
                    .send(Packet::SubAck(SubAck::new(
                        subscription.pkid,
                        vec![SubscribeReasonCode::QoS2; 2],
                    )))
                    .await;
                let Packet::Publish(list) = broker.next().await else {
                    panic!("initial device list expected after baseline SUBACK")
                };
                assert_eq!(list.topic, "master/proxy/getDevList");
                assert_eq!(c.clock.now(), Duration::from_secs(4));
            } else {
                assert!(broker.next_optional().await.is_none());
                until(|| {
                    matches!(
                        c.context.status.borrow().gateway,
                        GatewayStatus::Unavailable(_)
                    )
                })
                .await;
                assert_eq!(
                    c.context.status.borrow().gateway,
                    GatewayStatus::Unavailable(
                        crate::xiaomi::central::CentralError::Timeout.to_string()
                    )
                );
            }
        }));
    }
}

pub(super) fn observation(record: &CachedPhysicalDevice, online: bool) -> DeviceEntry {
    DeviceEntry {
        did: record.device.key().id.clone(),
        access: Ok(DeviceAccess {
            did: record.device.key().id.clone(),
            name: Some(format!("name {}", record.device.key().id)),
            model: record.model.clone(),
            instance_type: record.instance_type.clone(),
            online,
            spec_v2_access: true,
            push_available: true,
        }),
    }
}

#[test]
fn filtered_lists_only_update_requested_members_and_preserve_other_access() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            let context = &control.context;
            context.observe_access(
                control
                    .data
                    .iter()
                    .map(|record| observation(record, true))
                    .collect(),
            );
            let one = context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap();
            let multi = context
                .registry
                .get(&DeviceKey::new("xiaomi", "multi"))
                .unwrap();
            assert_eq!(one.availability(), Availability::Available);
            assert_eq!(multi.availability(), Availability::Available);

            context.observe_access_filtered(
                &["one".into(), "unknown".into()],
                vec![
                    observation(&control.data[0], false),
                    observation(&control.data[1], false),
                ],
            );
            assert!(matches!(one.availability(), Availability::Unavailable(_)));
            assert_eq!(multi.availability(), Availability::Available);
            assert!(
                context.live.borrow().devices["multi"]
                    .access
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .online
            );

            context.observe_access_filtered(
                &["one".into()],
                vec![observation(&control.data[0], true)],
            );
            assert_eq!(one.availability(), Availability::Available);
            context.observe_access_filtered(&["one".into()], Vec::new());
            assert!(context.live.borrow().devices["one"].access.is_none());
            assert_eq!(multi.availability(), Availability::Available);
        })
        .await;
    }));
}

#[test]
fn local_reconnects_use_six_to_sixty_seconds_and_reset_on_every_connection() {
    let mut reconnect = Backoff::new(6, 60);
    let actual = (0..7)
        .map(|_| reconnect.failed().as_secs())
        .collect::<Vec<_>>();
    assert_eq!(actual, [6, 12, 24, 48, 60, 60, 60]);
    reconnect = Backoff::new(6, 60);
    assert_eq!(reconnect.failed(), Duration::from_secs(6));

    let mut other = Backoff::new(1, 30);
    let actual = (0..7).map(|_| other.failed().as_secs()).collect::<Vec<_>>();
    assert_eq!(actual, [1, 2, 4, 8, 16, 30, 30]);
}
