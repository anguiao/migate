use super::*;

#[test]
fn mqtt_network_retry_caps_at_six_hundred_seconds_and_new_token_resets_it_immediately() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    peer.state
        .borrow_mut()
        .connacks
        .extend(std::iter::repeat_n(ConnectReturnCode::ServerUnavailable, 9));
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().connects.len() == 1).await;
            let mut cursor = 0;
            for (index, seconds) in [10, 20, 40, 80, 160, 320, 600].into_iter().enumerate() {
                let due = c.clock.now() + Duration::from_secs(seconds);
                until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&due)).await;
                cursor = c.clock.deadlines.lock().unwrap().len();
                c.clock.advance(Duration::from_secs(seconds));
                until(|| peer.state.borrow().connects.len() == index + 2).await;
                assert!(!c.context.http_paused.get());
            }
            let capped = c.clock.now() + Duration::from_secs(600);
            until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&capped)).await;
            let mut record = c.store.xiaomi().load().unwrap().unwrap();
            record.tokens.access_token = "replacement-token".into();
            c.store.xiaomi().replace(&record).unwrap();
            c.context.live.borrow_mut().credentials = Some(record);
            cursor = c.clock.deadlines.lock().unwrap().len();
            c.context.credentials_changed.notify(usize::MAX);
            until(|| peer.state.borrow().connects.len() == 9).await;
            assert_eq!(peer.state.borrow().connects[8], "replacement-token");
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&due)).await;
            c.clock.advance(Duration::from_secs(10));
            until(|| cloud_active(&c, "one")).await;
            let old = c
                .context
                .notifications
                .borrow()
                .as_ref()
                .unwrap()
                .handle
                .clone();
            cursor = c.clock.deadlines.lock().unwrap().len();
            peer.send(Packet::Disconnect(Disconnect::new()));
            until(|| c.context.notifications.borrow().is_none()).await;
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&due)).await;
            c.clock.advance(Duration::from_secs(10));
            until(|| cloud_active(&c, "one")).await;
            let one = function(&c.context.registry, "one", "main");
            until(|| one.snapshot().power() == Some(Power::Off)).await;
            c.context.property_value_event(
                &super::super::super::notifications::NotificationSource::Cloud(old),
                "one",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                &json!(true),
                None,
                false,
            );
            assert_eq!(one.snapshot().power(), Some(Power::Off));
            assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        })
        .await;
    }));
}

#[test]
fn a_real_cloud_online_recovery_reopens_one_exhausted_subscription_window() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    let property = "device/one/up/properties_changed/2/1";
    peer.state
        .borrow_mut()
        .reject
        .insert(property.into(), SubscribeReasonCode::QuotaExceeded);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| {
                peer.count(property) == 4
                    && c.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .test_cloud_exhausted()
            })
            .await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            peer.state.borrow_mut().reject.remove(property);
            peer.online("one", true);
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(peer.count(property), 4);
            peer.online("one", false);
            until(|| {
                !c.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .unwrap()
                    .online
            })
            .await;
            peer.online("one", true);
            until(|| cloud_active(&c, "one")).await;
            assert_eq!(peer.count(property), 5);
            assert!(!c.context.http_paused.get());
        })
        .await;
    }));
}

#[test]
fn held_online_subscription_cannot_retry_after_device_access_is_revoked() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    let online = "device/one/state/#";
    let property = "device/one/up/properties_changed/2/1";
    peer.state.borrow_mut().hold.insert(online.into());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| {
                cloud_active(&c, "one")
                    && peer.count(online) == 1
                    && peer
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|subscribe| subscribe.filters[0].path == online)
            })
            .await;
            assert_eq!(peer.count(property), 1);
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let token_requests = c.cloud.lock().unwrap().tokens;
            c.cloud.lock().unwrap().property_code = -704_012_904;
            let one = function(&c.context.registry, "one", "main");
            assert_eq!(
                one.execute(Command::On).await,
                Err(crate::device::CommandError::Rejected(
                    "cloud code -704012904".into()
                ))
            );
            until(|| {
                c.context.live.borrow().devices["one"].cloud_forbidden
                    && !cloud_active(&c, "one")
                    && peer.state.borrow().seen.iter().any(|packet| {
                        matches!(packet, Packet::Unsubscribe(unsubscribe)
                            if unsubscribe.filters.iter().any(|filter| filter == online))
                    })
            })
            .await;

            // A late ACK belongs to the cancelled preparation, even on the same connection.
            peer.release(online);
            async_io::Timer::after(Duration::from_millis(20)).await;
            c.clock.advance(Duration::from_secs(1));
            for _ in 0..20 {
                c.context.changed.notify(usize::MAX);
                future::yield_now().await;
            }
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(
                peer.count(online),
                1,
                "a retired online preparation must not subscribe the forbidden device again"
            );
            assert_eq!(peer.count(property), 1);
            assert_eq!(c.cloud.lock().unwrap().tokens, token_requests);
            assert!(c.context.notifications.borrow().is_some());
            assert!(
                c.context.live.borrow().devices["one"]
                    .admitted
                    .as_ref()
                    .unwrap()
                    .test_cloud_subscribed_empty()
            );
        })
        .await;
    }));
}
