//! Older reads cannot overwrite later state notifications.
use super::*;

#[test]
fn a_native_motion_event_invalidates_an_older_duration_read() {
    block_on(motion_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(motion_peer(), |peer| async move {
            until(|| state(control, "motion").active == Some(false)).await;
            settled(control, "motion", DURATION).await;
            peer.state
                .borrow_mut()
                .read_timeout
                .insert(("one".into(), 2, 1024));
            work(control).refresh_property("motion", DURATION);
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == 2 && payload["piid"] == 1024
                })
            })
            .await;
            event(&peer, false, Some(61));
            until(|| state(control, "motion").active == Some(true)).await;
            let observed = state(control, "motion");
            peer.release_property("one", DURATION, json!({"value":300,"updateTime":1}))
                .await;
            peer.notify_value("one", ILLUMINANCE, json!(9.0), false);
            until(|| illumination(control) == Some(9.0)).await;
            assert_eq!(state(control, "motion"), observed);
        })
        .await;
    }));
}

#[test]
fn native_motion_cloud_event_invalidates_an_older_local_read_and_allows_a_new_local_snapshot() {
    let (rig, listener, cloud) = cloud_rig(motion_rig());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let peer = motion_peer();
            peer.state
                .borrow_mut()
                .sub_reject
                .insert(("one".into(), 2, 1008));
            let control = &c;
            let cloud = &cloud;
            c.peer(peer, |peer| async move {
                cloud_ready(control, "motion").await;
                until(|| !work(control).refresh.borrow().contains("motion", DURATION)).await;
                let diagnostics = control.context.diagnostics();
                let function = diagnostics
                    .devices
                    .iter()
                    .find(|d| d.did == "one")
                    .unwrap()
                    .functions
                    .iter()
                    .find(|f| f.function == "motion")
                    .unwrap();
                assert_eq!(function.selected_read, Some(PathKind::Local));
                peer.state
                    .borrow_mut()
                    .read_timeout
                    .insert(("one".into(), 2, 1024));
                work(control).refresh_property("motion", DURATION);
                until(|| {
                    peer.state.borrow().held.iter().any(|(publish, _)| {
                        let payload = decode_mips(&publish.payload).unwrap().payload;
                        payload["siid"] == 2 && payload["piid"] == 1024
                    })
                })
                .await;
                cloud_event(cloud, Some(311));
                until(|| state(control, "motion").source_time == Some(311)).await;
                peer.release_property("one", DURATION, json!({"value":300}))
                    .await;
                peer.notify_value("one", ILLUMINANCE, json!(11.0), false);
                until(|| illumination(control) == Some(11.0)).await;
                assert_eq!(state(control, "motion").active, Some(true));
                assert_eq!(
                    work(control).observation_source("motion"),
                    Some(PathKind::Cloud)
                );
                peer.state.borrow_mut().read_timeout.clear();
                work(control).refresh_property("motion", DURATION);
                until(|| state(control, "motion").active == Some(false)).await;
                assert_eq!(
                    work(control).observation_source("motion"),
                    Some(PathKind::Local)
                );
                assert!(matches!(
                    work(control).function_observer("motion"),
                    Some(NotificationSource::Cloud(_))
                ));
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn native_motion_event_is_not_overwritten_by_an_older_held_http_read() {
    let (rig, listener, cloud) = cloud_rig(motion_rig());
    let (release, gate) = flume::bounded(1);
    rig.cloud
        .lock()
        .unwrap()
        .read_gates
        .insert(("one".into(), 2, 1024), gate);
    let topic = "device/one/up/event_occured/2/1008";
    cloud.state.borrow_mut().hold.insert(topic.into());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| {
                cloud_reads(&c, DURATION) >= 1
                    && cloud
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|sub| sub.filters[0].path == topic)
            })
            .await;
            cloud.release(topic);
            cloud_event(&cloud, Some(501));
            until(|| state(&c, "motion").source_time == Some(501)).await;
            c.cloud
                .lock()
                .unwrap()
                .read_gates
                .remove(&("one".into(), 2, 1024));
            release.send(()).unwrap();
            until(|| !work(&c).refresh.borrow().contains("motion", DURATION)).await;
            assert_eq!(state(&c, "motion").active, Some(true));
            assert_eq!(state(&c, "motion").source_time, Some(501));
        })
        .await;
    }));
}
