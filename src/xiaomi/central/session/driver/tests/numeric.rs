use super::*;

#[test]
fn boolean_reads_validate_the_type_before_classifying_the_business_result() {
    block_on(async {
        for value in [json!(true), json!(42)] {
            let (handle, mut driver, budget, _) = rig();
            let completed = Rc::new(RefCell::new(None));
            let callback = completed.clone();
            let read = handle.read_property("target", BINDING, move |outcome| {
                *callback.borrow_mut() = Some(outcome);
            });
            driver.drain_requests().unwrap();
            let mid = *driver.pending.keys().next().unwrap();
            write_ready(&mut driver, &budget, usize::MAX).await;
            reply(&mut driver, mid, json!({"value":value}));
            let outcome = read.await;
            let expected = if value.is_boolean() {
                RequestOutcome {
                    result: Ok(true),
                    stage: RequestStage::BusinessResult,
                    timeout: None,
                }
            } else {
                RequestOutcome {
                    result: Err(CentralError::Protocol),
                    stage: RequestStage::MayHaveBeenSent,
                    timeout: None,
                }
            };
            assert_eq!(outcome, expected);
            assert_eq!(*completed.borrow(), Some(expected));
        }
    });
}

#[test]
fn numeric_reads_keep_guarded_send_outcomes() {
    block_on(async {
        let (handle, mut driver, budget, _) = rig();
        let first = Rc::new(Cell::new(0));
        let sent = first.clone();
        let read = handle.read_property_value_with_timeout_guarded(
            "target",
            BINDING,
            Duration::from_secs(1),
            SendHooks::new(|| SendDecision::Send, move || sent.set(sent.get() + 1)),
            |_| {},
        );
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        reply(
            &mut driver,
            mid,
            json!({"did":"target","siid":2,"piid":1,"value":65535}),
        );
        let outcome = read.await;
        assert_eq!(outcome.result, Ok(json!(65535)));
        assert_eq!(outcome.stage, RequestStage::BusinessResult);
        assert_eq!(first.get(), 1);
    });
}

#[test]
fn guarded_batch_write_sends_once_and_callback_retains_partial_results_after_drop() {
    block_on(async {
        let (handle, mut driver, budget, _) = rig();
        let mode = PropertyBinding { siid: 4, piid: 2 };
        let first = Rc::new(Cell::new(0));
        let sent = first.clone();
        let completed = Rc::new(RefCell::new(None));
        let callback = completed.clone();
        let write = handle.set_properties_guarded(
            "target",
            &[(BINDING, json!(2700)), (mode, json!(0))],
            SendHooks::new(|| SendDecision::Send, move || sent.set(sent.get() + 1)),
            move |outcome| *callback.borrow_mut() = Some(outcome),
        );
        drop(write);
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        let mut frames = Frames::default();
        frames
            .push(driver.output.front().unwrap().frame.bytes())
            .unwrap();
        let Some(Packet::Publish(publish)) = frames.next_packet().unwrap() else {
            panic!("batch publication expected")
        };
        let message = wire::decode_mips(&publish.payload).unwrap();
        assert_eq!(publish.topic, "master/proxy/rpcReq");
        assert_eq!(message.payload["rpc"]["method"], "set_properties");
        assert_eq!(
            message.payload["rpc"]["params"],
            json!([
                {"did":"target","siid":2,"piid":1,"value":2700},
                {"did":"target","siid":4,"piid":2,"value":0}
            ])
        );
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(first.get(), 1);
        reply(
            &mut driver,
            mid,
            json!({"result":[{"did":"target","siid":4,"piid":2,"code":-704},{"did":"target","siid":2,"piid":1,"code":0}]}),
        );
        let completed = completed.borrow();
        let outcome = completed.as_ref().unwrap();
        assert_eq!(outcome.stage, RequestStage::BusinessResult);
        let result = outcome.result.as_ref().unwrap();
        assert!(!result.protocol_error);
        assert_eq!(
            result.items,
            vec![Ok(()), Err(CentralError::Rejected(-704))]
        );
        assert!(driver.pending.is_empty());
        assert_eq!(driver.publications.len(), 1);
    });
}

#[test]
fn general_property_reads_reject_invalid_values_and_present_identity_mismatches() {
    block_on(async {
        for body in [
            json!({"value":null}),
            json!({"value":"50"}),
            json!({"value":[]}),
            json!({"did":"foreign","value":50}),
            json!({"siid":3,"value":50}),
            json!({"piid":2,"value":50}),
        ] {
            let (handle, mut driver, budget, _) = rig();
            let read = handle.read_property_value("target", BINDING, |_| {});
            driver.drain_requests().unwrap();
            let mid = *driver.pending.keys().next().unwrap();
            write_ready(&mut driver, &budget, usize::MAX).await;
            reply(&mut driver, mid, body);
            let outcome = read.await;
            assert_eq!(outcome.result, Err(CentralError::Protocol));
            assert_eq!(outcome.stage, RequestStage::MayHaveBeenSent);
        }
    });
}

#[test]
fn batch_reply_identity_failures_preserve_confirmed_items() {
    block_on(async {
        for extra in [
            json!([]),
            json!([{"did":"target","siid":4,"piid":2,"code":0},{"did":"target","siid":4,"piid":2,"code":1}]),
            json!([{"did":"wrong","siid":4,"piid":2,"code":0}]),
            json!([{"did":"target","siid":9,"piid":1,"code":0}]),
        ] {
            let (handle, mut driver, budget, _) = rig();
            let mode = PropertyBinding { siid: 4, piid: 2 };
            let write = handle.set_properties(
                "target",
                &[(BINDING, json!(2700)), (mode, json!(0))],
                |_| {},
            );
            driver.drain_requests().unwrap();
            let mid = *driver.pending.keys().next().unwrap();
            write_ready(&mut driver, &budget, usize::MAX).await;
            let mut items = vec![json!({"did":"target","siid":2,"piid":1,"code":0})];
            items.extend(extra.as_array().unwrap().iter().cloned());
            reply(&mut driver, mid, json!({"result":items}));
            let result = write.await.result.unwrap();
            assert!(result.protocol_error);
            assert_eq!(result.items, vec![Ok(()), Err(CentralError::Protocol)]);
        }
    });
}

#[test]
fn numeric_notifications_reuse_received_property_generation() {
    block_on(async {
        let (handle, mut driver, _, _) = rig();
        ready_property(&handle, &mut driver, "target").await;
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut on_event = |event| events.borrow_mut().push(event);
        let packet = |pkid, value| {
            let mut publish = Publish::new(
                "client/appMsg/notify/iot/target/property/2.1",
                QoS::ExactlyOnce,
                wire::encode_mips(
                    1,
                    None,
                    &json!({"did":"target","siid":2,"piid":1,"value":value}),
                ),
            );
            publish.pkid = pkid;
            Packet::Publish(publish)
        };
        driver.packet(packet(80, 10), &mut on_event).unwrap();
        handle.invalidate_received_properties("target");
        driver.drain_property_invalidations();
        driver
            .packet(
                Packet::PubRel(mqttbytes::v5::PubRel::new(80)),
                &mut on_event,
            )
            .unwrap();
        assert!(events.borrow().is_empty());
        driver.packet(packet(81, 20), &mut on_event).unwrap();
        driver
            .packet(
                Packet::PubRel(mqttbytes::v5::PubRel::new(81)),
                &mut on_event,
            )
            .unwrap();
        assert_eq!(
            *events.borrow(),
            vec![CentralEvent::PropertyValue {
                did: "target".into(),
                binding: BINDING,
                value: json!(20)
            }]
        );
    });
}

#[test]
fn unrelated_batch_results_do_not_claim_business_result_progress() {
    block_on(async {
        let (handle, mut driver, budget, _) = rig();
        let write = handle.set_properties("target", &[(BINDING, json!(50))], |_| {});
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        reply(
            &mut driver,
            mid,
            json!({"result":[{"did":"foreign","siid":2,"piid":1,"code":0}]}),
        );
        let outcome = write.await;
        assert_eq!(outcome.stage, RequestStage::MayHaveBeenSent);
        let result = outcome.result.unwrap();
        assert!(result.protocol_error);
        assert_eq!(result.items, vec![Err(CentralError::Protocol)]);
    });
}

#[test]
fn guarded_batch_withdrawal_is_unsent_and_does_not_fire_first_send() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let sent = Rc::new(Cell::new(0));
        let first = sent.clone();
        let write = handle.set_properties_guarded(
            "target",
            &[(BINDING, json!(50))],
            SendHooks::new(
                || SendDecision::Withdraw,
                move || first.set(first.get() + 1),
            ),
            |_| {},
        );
        driver.drain_requests().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(write.await, unsent(CentralError::Withdrawn));
        assert_eq!(written.get(), 0);
        assert_eq!(sent.get(), 0);
    });
}

#[test]
fn invalid_general_property_inputs_fail_before_queuing() {
    block_on(async {
        let (handle, _, _, _) = rig();
        for items in [
            vec![],
            vec![(BINDING, json!(true)), (BINDING, json!(false))],
            vec![(BINDING, json!(null))],
            vec![(BINDING, json!("50"))],
            vec![(BINDING, json!({}))],
            vec![(PropertyBinding { siid: 0, piid: 1 }, json!(10))],
        ] {
            assert_eq!(
                handle.set_properties("target", &items, |_| {}).await,
                unsent(CentralError::InvalidInput)
            );
        }
        assert!(handle.shared.queue.borrow().is_empty());
    });
}

#[test]
fn unsolicited_batch_result_marks_invalid_without_losing_known_success() {
    block_on(async {
        let (handle, mut driver, budget, _) = rig();
        let write = handle.set_properties("target", &[(BINDING, json!(50))], |_| {});
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        reply(
            &mut driver,
            mid,
            json!({"result":[{"did":"target","siid":2,"piid":1,"code":0},{"did":"target","siid":9,"piid":1,"code":0}]}),
        );
        let outcome = write.await;
        assert_eq!(outcome.stage, RequestStage::BusinessResult);
        let result = outcome.result.unwrap();
        assert!(result.protocol_error);
        assert_eq!(result.items, vec![Ok(())]);
    });
}
