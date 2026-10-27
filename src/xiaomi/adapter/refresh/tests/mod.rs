use super::*;

#[test]
fn dirty_again_produces_exactly_one_follow_up_read() {
    let mut state = RefreshState::default();
    let binding = PropertyBinding { siid: 2, piid: 1 };
    let key = super::super::observations::key("main", binding);
    state.dirty_again.insert(key.clone());
    assert!(state.finish_dirty("main", binding));
    assert!(!state.dirty_again.contains(&key));
    assert_eq!(state.pending.len(), 1);
    assert!(!state.finish_dirty("main", binding));
    assert_eq!(state.pending.len(), 1);
}

fn cloud_path() -> ControlPath {
    ControlPath::Cloud(super::super::routing::CloudPath {
        uid: "account".into(),
        access_token: "token".into(),
    })
}

#[test]
fn a_satisfied_reserved_baseline_is_not_requeued_when_withdrawn() {
    let mut state = RefreshState::default();
    let binding = PropertyBinding {
        siid: 2,
        piid: 1024,
    };
    state.request_baseline("motion", binding);
    let (read, _) = state
        .reserve(Duration::ZERO, |_| Some(cloud_path()), false)
        .unwrap();

    state.satisfy_baseline("motion");
    state.complete(read, ReadResult::Retired, Duration::ZERO, false);

    assert!(state.pending.is_empty());
}

fn motion_work() -> Rc<DeviceWork> {
    use crate::{
        device::{DeviceKey, PhysicalDevice},
        xiaomi::capability::{NativeDescription, parse_device_for_model},
    };
    let instance: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/xiaomi-pir1.json"
    ))
    .unwrap();
    let native = NativeDescription::new(
        "xiaomi.motion.pir1".into(),
        instance["type"].as_str().unwrap().into(),
        parse_device_for_model(
            DeviceKey::new("xiaomi", "one"),
            "xiaomi.motion.pir1",
            &instance,
        )
        .unwrap(),
    )
    .unwrap();
    let (physical, _) = PhysicalDevice::new(native.device().physical_description("Motion"));
    Rc::new(DeviceWork::new(
        native,
        Rc::new(super::super::state::LogicalSink::new(physical)),
        Rc::new(event_listener::Event::new()),
        None,
    ))
}

#[test]
fn an_unpolled_baseline_lease_is_not_returned_after_notification() {
    let work = motion_work();
    let binding = PropertyBinding {
        siid: 2,
        piid: 1024,
    };
    let (read, flight) = {
        let mut state = work.refresh.borrow_mut();
        state.request_baseline("motion", binding);
        state
            .reserve(Duration::ZERO, |_| Some(cloud_path()), false)
            .unwrap()
    };
    // This lease exists before the read future's first poll.
    let cleanup = ReadCleanup {
        work: work.clone(),
        flight,
        read,
        retry: true,
    };
    work.refresh.borrow_mut().satisfy_baseline("motion");
    drop(cleanup);

    let state = work.refresh.borrow();
    assert!(state.pending.is_empty());
    assert!(state.inflight.is_empty());
}

#[test]
fn baseline_satisfaction_preserves_ordinary_and_new_source_demands() {
    let binding = PropertyBinding {
        siid: 2,
        piid: 1024,
    };
    for ordinary in [false, true] {
        let mut state = RefreshState::default();
        if ordinary {
            state.request("motion", binding);
        } else {
            state.request_baseline("motion", binding);
        }
        let (read, _) = state
            .reserve(Duration::ZERO, |_| Some(cloud_path()), false)
            .unwrap();
        if ordinary {
            state.satisfy_baseline("motion");
        }
        state.complete(read, ReadResult::Retired, Duration::ZERO, false);
        assert_eq!(state.pending.len(), 1);
    }

    let mut state = RefreshState::default();
    state.request_baseline("motion", binding);
    let (old, _) = state
        .reserve(Duration::ZERO, |_| Some(cloud_path()), false)
        .unwrap();
    state.satisfy_baseline("motion");
    state.request_baseline("motion", binding);
    state.complete(old, ReadResult::Retired, Duration::ZERO, false);
    assert_eq!(state.pending.len(), 1);
    state.inflight.clear();
    let (new, _) = state
        .reserve(Duration::ZERO, |_| Some(cloud_path()), false)
        .unwrap();
    state.complete(new, ReadResult::Retired, Duration::ZERO, false);
    assert_eq!(state.pending.len(), 1);

    state.request("motion", binding);
    state.satisfy_baseline("motion");
    assert_eq!(state.pending.len(), 1);
}

fn returned_ordinary_demand_survives_a_pending_baseline(drop_lease: bool) {
    let work = motion_work();
    let binding = PropertyBinding {
        siid: 2,
        piid: 1024,
    };
    let (read, flight) = {
        let mut state = work.refresh.borrow_mut();
        state.request("motion", binding);
        state
            .reserve(Duration::ZERO, |_| Some(cloud_path()), false)
            .unwrap()
    };
    let mut cleanup = ReadCleanup {
        work: work.clone(),
        flight,
        read: read.clone(),
        retry: true,
    };
    work.refresh
        .borrow_mut()
        .request_baseline("motion", binding);
    if !drop_lease {
        work.refresh
            .borrow_mut()
            .complete(read, ReadResult::Retired, Duration::ZERO, false);
        cleanup.retry = false;
    }
    drop(cleanup);

    let mut state = work.refresh.borrow_mut();
    state.satisfy_baseline("motion");
    assert!(
        state
            .reserve(Duration::ZERO, |_| Some(cloud_path()), false)
            .is_some()
    );
}

#[test]
fn a_returned_ordinary_read_keeps_priority_over_a_pending_baseline() {
    returned_ordinary_demand_survives_a_pending_baseline(false);
}

#[test]
fn a_returned_ordinary_lease_keeps_priority_over_a_pending_baseline() {
    returned_ordinary_demand_survives_a_pending_baseline(true);
}
