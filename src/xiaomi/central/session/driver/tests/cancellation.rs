use super::*;

#[test]
fn cancelling_unsent_business_keeps_writes_and_subscriptions() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, budget, written) = rig();
            let callbacks = Rc::new(Cell::new(0));
            let list = handle.get_devices(count_result(&callbacks));
            let filtered = handle
                .get_devices_filtered(&["target".into(), "other".into()], count_result(&callbacks));
            let device = handle.get_devices_filtered(&["target".into()], count_result(&callbacks));
            let read = handle.read_property_value("target", BINDING, count_result(&callbacks));
            let write = handle.set_property("target", BINDING, true, |_| {});
            let subscription = handle.subscribe_property("target", BINDING);
            if dispatched {
                driver.drain_requests().unwrap();
            }
            handle.cancel_unsent_business();
            driver.drain_cancellations();
            driver.drain_requests().unwrap();
            assert_eq!(list.await, unsent(CentralError::Cancelled));
            assert_eq!(filtered.await, unsent(CentralError::Cancelled));
            assert_eq!(device.await, unsent(CentralError::Cancelled));
            assert_eq!(read.await, unsent(CentralError::Cancelled));
            assert_eq!(callbacks.get(), 4);
            assert_eq!(written.get(), 0);
            assert_eq!(handle.shared.outstanding.get(), 2);
            assert_eq!(driver.pending.len(), 1);
            assert_eq!(driver.publications.len(), 1);
            assert_eq!(driver.subscriptions.len(), 1);
            assert_eq!(driver.output.len(), 2);
            let mid = *driver.pending.keys().next().unwrap();
            assert_eq!(driver.pending[&mid].kind.operation(), "set");
            let subscription_id = *driver.subscriptions.keys().next().unwrap();
            write_ready(&mut driver, &budget, usize::MAX).await;
            write_ready(&mut driver, &budget, usize::MAX).await;
            reply(
                &mut driver,
                mid,
                json!({"result":[{"did":"target","code":0}]}),
            );
            driver
                .packet(
                    Packet::SubAck(SubAck::new(
                        subscription_id,
                        vec![SubscribeReasonCode::QoS2],
                    )),
                    &mut |_| {},
                )
                .unwrap();
            assert_eq!(write.await.result, Ok(()));
            subscription.await.unwrap();
            assert_eq!(callbacks.get(), 4);
            assert_eq!(handle.shared.outstanding.get(), 0);
            assert_eq!(driver.property_topics.len(), 1);
        }
    });
}

#[test]
fn cancelling_unsent_business_preserves_partial_reads_and_qos2_frames() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let read = handle.read_property_value("target", BINDING, |_| {});
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        let pkid = *driver.publications.keys().next().unwrap();
        write_ready(&mut driver, &budget, 3).await;
        let list = handle.get_devices(|_| {});
        handle.cancel_unsent_business();
        driver.drain_cancellations();
        assert_eq!(list.await, unsent(CentralError::Cancelled));
        assert_eq!(driver.output.front().unwrap().frame.offset(), 3);
        assert!(driver.pending.contains_key(&mid));
        assert!(driver.publications.contains_key(&pkid));
        write_ready(&mut driver, &budget, usize::MAX).await;
        driver
            .packet(
                Packet::PubRec(mqttbytes::v5::PubRec::new(pkid)),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(driver.output.len(), 1);
        handle.cancel_unsent_business();
        driver.drain_cancellations();
        assert_eq!(driver.output.len(), 1);
        assert!(driver.publications.contains_key(&pkid));
        write_ready(&mut driver, &budget, usize::MAX).await;
        driver
            .packet(
                Packet::PubComp(mqttbytes::v5::PubComp::new(pkid)),
                &mut |_| {},
            )
            .unwrap();
        assert!(driver.publications.is_empty());
        reply(&mut driver, mid, json!({"value":true}));
        assert_eq!(read.await.result, Ok(json!(true)));
        assert!(written.get() > 3);
        assert_eq!(handle.shared.outstanding.get(), 0);
    });
}

#[test]
fn cancelling_unsent_business_releases_capacity_once() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, _, _) = rig();
            let callbacks = Rc::new(Cell::new(0));
            for _ in 0..31 {
                drop(handle.get_devices(count_result(&callbacks)));
                drop(handle.read_property_value("target", BINDING, count_result(&callbacks)));
            }
            let write = handle.set_property("target", BINDING, true, |_| {});
            let subscription = handle.subscribe_property("target", BINDING);
            assert_eq!(handle.shared.outstanding.get(), LIMIT);
            assert_eq!(
                handle.set_property("full", BINDING, false, |_| {}).await,
                unsent(CentralError::QueueFull)
            );
            if dispatched {
                driver.drain_requests().unwrap();
            }
            handle.cancel_unsent_business();
            driver.drain_cancellations();
            assert_eq!(callbacks.get(), LIMIT - 2);
            assert_eq!(handle.shared.outstanding.get(), 2);
            handle.cancel_unsent_business();
            driver.drain_cancellations();
            assert_eq!(callbacks.get(), LIMIT - 2);
            assert_eq!(handle.shared.outstanding.get(), 2);
            let replacement = handle.read_property_value("fresh", BINDING, |_| {});
            assert_eq!(handle.shared.outstanding.get(), 3);
            driver.drain_requests().unwrap();
            driver.finish_all(CentralError::Cancelled);
            assert_eq!(replacement.await, unsent(CentralError::Cancelled));
            assert_eq!(write.await, unsent(CentralError::Cancelled));
            assert_eq!(subscription.await, Err(CentralError::Cancelled));
            assert_eq!(callbacks.get(), LIMIT - 2);
            assert_eq!(handle.shared.outstanding.get(), 0);
        }
    });
}

#[test]
fn a_response_timeout_callback_cancels_unsent_lists_before_the_same_socket_poll() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let observed = callbacks.clone();
        let cancel = handle.clone();
        let write = handle.set_property("target", BINDING, true, move |outcome| {
            observed.borrow_mut().push(outcome);
            cancel.cancel_unsent_business();
        });
        driver.drain_requests().unwrap();
        let write_id = *driver.pending.keys().next().unwrap();
        let publication_id = *driver.publications.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(
            driver.pending[&write_id].stage,
            RequestStage::MayHaveBeenSent
        );
        let sent = written.get();
        assert!(sent > 0);

        let list_callbacks = Rc::new(Cell::new(0));
        let list = handle.get_devices(count_result(&list_callbacks));
        driver.drain_requests().unwrap();
        assert_eq!(driver.output.len(), 1);
        assert_eq!(driver.output.front().unwrap().frame.offset(), 0);
        driver.pending.get_mut(&write_id).unwrap().deadline =
            Instant::now() - Duration::from_secs(1);
        driver
            .publications
            .get_mut(&publication_id)
            .unwrap()
            .deadline = Instant::now() + Duration::from_secs(60);

        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(written.get(), sent, "the cancelled list reached the socket");
        assert_eq!(list.await, unsent(CentralError::Cancelled));
        let expected = RequestOutcome {
            result: Err(CentralError::Timeout),
            stage: RequestStage::MayHaveBeenSent,
            timeout: Some(RequestTimeout::Response),
        };
        assert_eq!(write.await, expected);
        assert_eq!(
            callbacks.borrow().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(list_callbacks.get(), 1);
        assert!(driver.pending.is_empty());
        assert!(driver.output.is_empty());
        assert_eq!(driver.publications.len(), 1);
        assert!(driver.publications.contains_key(&publication_id));
        assert_eq!(handle.shared.outstanding.get(), 0);
        driver.finish_all(CentralError::Cancelled);
        assert_eq!(callbacks.borrow().len(), 1);
        assert_eq!(list_callbacks.get(), 1);
    });
}

#[test]
fn detailed_cancellation_removes_unsent_frames_without_replay() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, budget, written) = rig();
            let write = handle.set_property("target", BINDING, true, |_| {});
            if dispatched {
                driver.drain_requests().unwrap();
            }
            handle.cancel_device("target").unwrap();
            driver.drain_cancellations();
            assert_eq!(
                write.await,
                RequestOutcome {
                    result: Err(CentralError::Cancelled),
                    stage: RequestStage::NotSent,
                    timeout: None
                }
            );
            write_ready(&mut driver, &budget, usize::MAX).await;
            assert_eq!(written.get(), 0);
            assert!(driver.pending.is_empty());
        }
    });
}
