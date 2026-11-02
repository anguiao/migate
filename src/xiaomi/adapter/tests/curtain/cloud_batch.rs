use super::*;
use std::sync::atomic::Ordering;

#[test]
fn foreground_batch_keeps_successful_properties_when_another_fails() {
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            c.clock.automatic.store(false, Ordering::Relaxed);
            let before = c.cloud.lock().unwrap().read_batches.len();
            {
                let mut state = c.cloud.lock().unwrap();
                state.raw_values.insert(("one".into(), 2, 2), json!(1));
                state.raw_values.insert(("one".into(), 2, 3), json!(44));
                state.raw_values.insert(("one".into(), 2, 4), json!(66));
                state.read_codes.insert(("one".into(), 2, 3), -704_030_023);
            }
            let work = work(&c);
            for binding in [STATUS, CURRENT, TARGET] {
                work.refresh_property("curtain:2", binding);
            }
            work.refresh
                .borrow_mut()
                .exhaust_pending("curtain:2", CURRENT);
            c.clock.advance(Duration::from_millis(201));
            until(|| {
                curtain(&c).status.value == Some(CurtainMotion::Closing)
                    && curtain(&c).target_position.value == Some(66)
                    && work
                        .refresh
                        .borrow()
                        .property_problem("curtain:2", CURRENT)
                        .is_some()
                    && [STATUS, CURRENT, TARGET]
                        .iter()
                        .all(|binding| !work.refresh.borrow().contains("curtain:2", *binding))
            })
            .await;
            assert_eq!(curtain(&c).current_position.value, Some(25));
            assert!(!c.context.http_paused.get());
            let batches = &c.cloud.lock().unwrap().read_batches[before..];
            assert_eq!(batches.len(), 1);
            assert_eq!(batches[0].len(), 3);
            assert!([STATUS, CURRENT, TARGET].iter().all(|binding| {
                batches[0].contains(&("one".into(), binding.siid, binding.piid))
            }));
        })
        .await;
    }));
}

#[test]
fn in_flight_baselines_keep_a_new_notification_and_other_readings() {
    let (rig, listener, cloud) = cloud_rig();
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            c.clock.automatic.store(false, Ordering::Relaxed);
            let before = c.cloud.lock().unwrap().read_batches.len();
            {
                let mut state = c.cloud.lock().unwrap();
                state.raw_values.insert(("one".into(), 2, 2), json!(1));
                state.raw_values.insert(("one".into(), 2, 3), json!(44));
                state.raw_values.insert(("one".into(), 2, 4), json!(66));
                state.read_gates.insert(("one".into(), 2, 3), gate);
            }
            let work = work(&c);
            for binding in [STATUS, CURRENT, TARGET] {
                work.refresh
                    .borrow_mut()
                    .request_baseline("curtain:2", binding);
            }
            work.refresh_changed.notify(usize::MAX);
            c.clock.advance(Duration::from_millis(201));
            until(|| c.cloud.lock().unwrap().read_batches.len() > before).await;
            let source = work.function_observer("curtain:2").unwrap();
            c.context
                .property_value_event(&source, "one", CURRENT, &json!(64), Some(701), false);
            assert_eq!(curtain(&c).current_position.value, Some(64));
            release.send(()).unwrap();
            until(|| {
                curtain(&c).status.value == Some(CurtainMotion::Closing)
                    && curtain(&c).target_position.value == Some(66)
                    && [STATUS, CURRENT, TARGET]
                        .iter()
                        .all(|binding| !work.refresh.borrow().contains("curtain:2", *binding))
            })
            .await;
            assert_eq!(curtain(&c).current_position.value, Some(64));
            assert_eq!(curtain(&c).current_position.source_time, Some(701));
        })
        .await;
    }));
}

#[test]
fn unrequested_single_result_is_a_protocol_failure() {
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            c.clock.automatic.store(false, Ordering::Relaxed);
            c.cloud.lock().unwrap().read_results = Some(vec![
                json!({"did":"one","siid":2,"piid":2,"code":0,"value":1}),
                json!({"did":"foreign","siid":2,"piid":2,"code":0,"value":1}),
            ]);
            let work = work(&c);
            work.refresh_property("curtain:2", STATUS);
            work.refresh
                .borrow_mut()
                .exhaust_pending("curtain:2", STATUS);
            c.clock.advance(Duration::from_millis(201));
            until(|| {
                work.refresh
                    .borrow()
                    .property_problem("curtain:2", STATUS)
                    .is_some()
            })
            .await;
            assert_eq!(curtain(&c).status.value, Some(CurtainMotion::Stopped));
            assert!(
                work.refresh
                    .borrow()
                    .property_problem("curtain:2", STATUS)
                    .unwrap()
                    .contains("invalid cloud response")
            );
        })
        .await;
    }));
}

#[test]
fn invalid_batch_keeps_only_uniquely_matched_properties() {
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            c.clock.automatic.store(false, Ordering::Relaxed);
            c.cloud.lock().unwrap().read_results = Some(vec![
                json!({"did":"one","siid":2,"piid":2,"code":0,"value":1}),
                json!({"did":"one","siid":2,"piid":3,"code":0,"value":44}),
                json!({"did":"one","siid":2,"piid":3,"code":0,"value":45}),
                json!({"did":"foreign","siid":2,"piid":4,"code":0,"value":66}),
            ]);
            let work = work(&c);
            for binding in [STATUS, CURRENT, TARGET] {
                work.refresh_property("curtain:2", binding);
                work.refresh
                    .borrow_mut()
                    .exhaust_pending("curtain:2", binding);
            }
            c.clock.advance(Duration::from_millis(201));
            until(|| {
                curtain(&c).status.value == Some(CurtainMotion::Closing)
                    && work
                        .refresh
                        .borrow()
                        .property_problem("curtain:2", CURRENT)
                        .is_some()
                    && work
                        .refresh
                        .borrow()
                        .property_problem("curtain:2", TARGET)
                        .is_some()
            })
            .await;
            assert_eq!(curtain(&c).current_position.value, Some(25));
            assert_eq!(curtain(&c).target_position.value, Some(75));
            assert!(
                work.refresh
                    .borrow()
                    .property_problem("curtain:2", STATUS)
                    .is_none()
            );
        })
        .await;
    }));
}
