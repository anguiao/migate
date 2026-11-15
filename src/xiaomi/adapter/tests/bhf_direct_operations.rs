use super::*;
use crate::{
    device::FanCommand,
    storage::LanRecord,
    xiaomi::{
        adapter::{StepReceipt, model::PathKind},
        capability::{PropertyBinding, ReadInput, SHARED_FEEDBACK_CHANNEL},
        lan::{Token, tests::support::LegacyDevice},
    },
};
use std::sync::atomic::Ordering;

const DID: &str = "743765151";
const MODEL: &str = "yeelink.bhf_light.v13";
const BLOW: &str = "fan:3:2";
const HEATER: &str = "heater:3";
const BLOW_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 2 };
const VENTILATION_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 4 };
const HEATING: PropertyBinding = PropertyBinding { siid: 3, piid: 3 };
const TARGET: PropertyBinding = PropertyBinding { siid: 3, piid: 8 };
const MODE: PropertyBinding = PropertyBinding { siid: 3, piid: 1 };

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

fn setup(instance: Value) -> (Rig, LegacyDevice) {
    let rig =
        Rig::custom_lights(&[(DID, MODEL, instance)]).with_stall_timeout(Duration::from_secs(20));
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = LegacyDevice::bhf_v13(token.clone(), DID.parse().unwrap());
    rig.store
        .lan()
        .save(&LanRecord {
            scope: rig
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .scope
                .unwrap(),
            did: DID.into(),
            model: MODEL.into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.network.lan_port.set(peer.address.port());
    rig.clock.automatic.store(true, Ordering::Relaxed);
    (rig, peer)
}

fn function(c: &Control, id: &str) -> crate::device::Function {
    super::support::function(&c.context.registry, DID, id)
}

fn work(c: &Control) -> Rc<crate::xiaomi::adapter::work::DeviceWork> {
    c.context.live.borrow().devices[DID]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}

fn direct_requests(peer: &LegacyDevice, method: &str) -> Vec<Value> {
    peer.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["method"] == method)
        .cloned()
        .collect()
}

fn status_text(c: &Control) -> String {
    let status = crate::terminal::AuthStatus::new(
        c.context.status.borrow().auth.clone(),
        "/bin/migate".into(),
        "/data".into(),
    );
    status.update_adapter(c.context.diagnostics());
    let terminal = crate::terminal::bridge::Context::committed(c.publication.clone(), &status);
    block_on(terminal.handle_line(&format!("status {DID}"), 0))
        .unwrap()
        .unwrap()
}

async fn eventually(mut predicate: impl FnMut() -> bool) {
    let ok = future::or(
        async {
            async_io::Timer::after(Duration::from_secs(15)).await;
            false
        },
        async {
            until(&mut predicate).await;
            true
        },
    )
    .await;
    assert!(ok, "timed out waiting for direct bath-heater behavior");
}

async fn settle(c: &Control) {
    let device = work(c);
    eventually(|| {
        let refresh = device.refresh.borrow();
        let snapshot = refresh.snapshot();
        snapshot.pending.is_empty() && snapshot.inflight.is_empty()
    })
    .await;
    c.clock.automatic.store(false, Ordering::Relaxed);
}

fn standard_peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    for (binding, value) in [
        (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
        (PropertyBinding { siid: 2, piid: 2 }, json!(0)),
        (PropertyBinding { siid: 2, piid: 3 }, json!(50)),
        (BLOW_POWER, json!(true)),
        (VENTILATION_POWER, json!(true)),
        (HEATING, json!(false)),
        (TARGET, json!(25)),
        (MODE, json!(0)),
    ] {
        peer.value(DID, binding, value);
    }
    peer
}

#[test]
fn direct_low_speed_success_records_its_receipt_and_refreshes_related_properties() {
    let (rig, peer) = setup(v13());
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
                && work(&c)
                    .shared_feedback()
                    .temperature
                    .as_ref()
                    .is_some_and(|temperature| temperature.value == json!(26.5))
        })
        .await;
        settle(&c).await;
        let before = peer.requests.lock().unwrap().len();

        blow.begin_fan(FanCommand::Speed(1))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        let status = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap();
        let last = status
            .commands
            .into_iter()
            .find(|command| command.channel == BLOW)
            .unwrap()
            .last
            .unwrap();
        assert_eq!(last.actual_path, Some(PathKind::Direct));
        assert!(
            matches!(last.steps[0].receipt, StepReceipt::Accepted),
            "a successful direct low-speed command needs an Accepted receipt: {last:?}"
        );
        let expected = [HEATING, TARGET, BLOW_POWER, VENTILATION_POWER, MODE];
        let mut fresh = Vec::new();
        for _ in 0..10 {
            c.clock.advance(Duration::from_millis(250));
            async_io::Timer::after(Duration::from_millis(40)).await;
            fresh = peer.requests.lock().unwrap()[before..].to_vec();
            let standard_ready = expected.iter().all(|binding| {
                fresh.iter().any(|request| {
                    request["method"] == "get_properties"
                        && request["params"].as_array().unwrap().iter().any(|item| {
                            item["siid"] == json!(binding.siid)
                                && item["piid"] == json!(binding.piid)
                        })
                })
            });
            let shared_ready = fresh.iter().any(|request| {
                request["method"] == "get_prop"
                    && request["params"] == json!(["bh_mode", "fan_speed_idx", "temperature"])
            });
            if standard_ready && shared_ready {
                break;
            }
        }
        let shared_read = fresh.iter().any(|request| {
            request["method"] == "get_prop"
                && request["params"] == json!(["bh_mode", "fan_speed_idx", "temperature"])
        });
        assert!(shared_read, "no shared readback was scheduled: {fresh:?}");
        for binding in expected {
            assert!(
                fresh.iter().any(|request| {
                    request["method"] == "get_properties"
                        && request["params"].as_array().unwrap().iter().any(|item| {
                            item["siid"] == json!(binding.siid)
                                && item["piid"] == json!(binding.piid)
                        })
                }),
                "missing related standard read {binding:?}: {fresh:?}"
            );
        }
    }));
}

#[test]
fn legacy_command_business_ack_mismatch_keeps_the_mixed_direct_connection() {
    let (rig, peer) = setup(v13());
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
        })
        .await;
        c.clock.automatic.store(false, Ordering::Relaxed);
        *peer.ack.lock().unwrap() = json!(["unexpected"]);

        assert!(
            blow.begin_fan(FanCommand::Speed(1))
                .unwrap()
                .accepted()
                .await
                .is_err()
        );
        assert!(
            c.context.direct_candidate(DID).is_ok(),
            "an unpacked business acknowledgement shape error must not revoke the direct client"
        );
        let status = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap();
        let last = status
            .commands
            .into_iter()
            .find(|command| command.channel == BLOW)
            .unwrap()
            .last
            .unwrap();
        assert_eq!(last.actual_path, Some(PathKind::Direct));
        assert!(matches!(last.steps[0].receipt, StepReceipt::Uncertain(_)));
        assert!(c.cloud.lock().unwrap().action_requests.is_empty());

        *peer.ack.lock().unwrap() = json!(["ok"]);
        let before = peer.requests.lock().unwrap().len();
        blow.begin_fan(FanCommand::Power(false))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        let fresh = peer.requests.lock().unwrap()[before..].to_vec();
        assert!(fresh.iter().any(|request| {
            request["method"] == "set_properties"
                && request["params"][0]["siid"] == json!(3)
                && request["params"][0]["piid"] == json!(2)
                && request["params"][0]["value"] == json!(false)
        }));
        let reads = direct_requests(&peer, "get_prop").len();
        work(&c).refresh_shared_feedback();
        let mut read_started = false;
        for _ in 0..10 {
            if direct_requests(&peer, "get_prop").len() > reads {
                read_started = true;
                break;
            }
            c.clock.advance(Duration::from_millis(250));
            async_io::Timer::after(Duration::from_millis(40)).await;
        }
        assert!(read_started, "the shared read did not start");
        assert_eq!(work(&c).shared_feedback().source, Some(PathKind::Direct));
    }));
}

#[test]
fn failed_direct_shared_read_after_cloud_switch_does_not_consume_new_baseline() {
    let (rig, peer) = setup(v13());
    rig.network.lan_timeout.set(Duration::from_millis(100));
    let (rig, listener, cloud) = with_cloud_rig(rig);
    {
        let mut state = rig.cloud.lock().unwrap();
        state
            .rpc_results
            .insert("get_prop".into(), json!(["dry", 13, 27.0]));
    }
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let heater = function(&c, HEATER);
            eventually(|| {
                c.context.direct_candidate(DID).is_ok()
                    && c.context.cloud_candidate(DID, true).is_ok()
                    && work(&c).shared_feedback().source == Some(PathKind::Direct)
                    && heater
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
            })
            .await;
            settle(&c).await;

            peer.hold_read.store(true, Ordering::Relaxed);
            let before = direct_requests(&peer, "get_prop").len();
            work(&c).refresh_shared_feedback();
            let mut started = false;
            for _ in 0..10 {
                c.clock.advance(Duration::from_millis(250));
                async_io::Timer::after(Duration::from_millis(30)).await;
                if direct_requests(&peer, "get_prop").len() > before {
                    started = true;
                    break;
                }
            }
            assert!(started, "the old direct shared read did not start");

            // A newer demand exists while the old direct read is still in
            // flight. The old timeout must not record a problem for it, and
            // the replacement cloud request must be observed within 2 seconds.
            work(&c).refresh_shared_feedback();
            let (release, gate) = flume::bounded(1);
            c.cloud.lock().unwrap().rpc_gate = Some(gate);
            let rpc_before = c.cloud.lock().unwrap().rpc_reads.len();
            let mut switched = false;
            for _ in 0..8 {
                c.clock.advance(Duration::from_millis(250));
                async_io::Timer::after(Duration::from_millis(30)).await;
                if c.context.direct_candidate(DID).is_err()
                    && matches!(
                        c.context.read_path_for_input(
                            DID,
                            SHARED_FEEDBACK_CHANNEL,
                            ReadInput::SharedFeedback
                        ),
                        Some(crate::xiaomi::adapter::routing::ControlPath::Cloud(_))
                    )
                    && c.cloud.lock().unwrap().rpc_reads.len() > rpc_before
                {
                    switched = true;
                    break;
                }
            }
            assert!(
                switched,
                "the new cloud baseline was not requested within 2 seconds"
            );
            assert!(
                work(&c)
                    .refresh
                    .borrow()
                    .input_problem(SHARED_FEEDBACK_CHANNEL, ReadInput::SharedFeedback)
                    .is_none(),
                "the old failed direct flight must not record a problem for the new cloud baseline"
            );

            c.cloud.lock().unwrap().rpc_gate = None;
            release.send(()).unwrap();
            peer.hold_read.store(false, Ordering::Relaxed);
            let mut applied = false;
            for _ in 0..8 {
                c.clock.advance(Duration::from_millis(250));
                async_io::Timer::after(Duration::from_millis(30)).await;
                if work(&c).shared_feedback().source == Some(PathKind::Cloud)
                    && heater
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.current_temperature.value == Some(27.0))
                    && work(&c)
                        .refresh
                        .borrow()
                        .input_problem(SHARED_FEEDBACK_CHANNEL, ReadInput::SharedFeedback)
                        .is_none()
                {
                    applied = true;
                    break;
                }
            }
            assert!(
                applied,
                "the new cloud baseline was not observed within 2 seconds"
            );
        })
        .await;
    }));
}

#[test]
fn diagnostics_show_low_speed_and_shared_candidates_without_hub() {
    let (rig, _direct) = setup(v13());
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |_hub| async move {
                eventually(|| {
                    control.context.direct_candidate(DID).is_ok()
                        && work(control).function_observer(HEATER).is_some()
                        && work(control).shared_feedback().source.is_some()
                })
                .await;
                let text = status_text(control);
                assert!(
                    text.contains(
                        "Low-speed control: candidates direct, cloud; selected direct."
                    ),
                    "{text}"
                );
                assert!(
                    text.contains(
                        "Legacy feedback group: candidates direct, cloud; selected read direct; last observed direct;"
                    ),
                    "{text}"
                );
                assert!(
                    !text.contains("Low-speed control: candidates hub"),
                    "{text}"
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn low_speed_diagnostics_use_cloud_when_direct_is_unavailable() {
    let (rig, _listener, _cloud) = with_cloud_rig(Rig::custom_lights(&[(DID, MODEL, v13())]));
    block_on(rig.run(|c| async move {
        eventually(|| {
            c.context.direct_candidate(DID).is_err()
                && matches!(
                    c.context.low_speed_path(DID, BLOW),
                    Some(crate::xiaomi::adapter::routing::ControlPath::Cloud(_))
                )
                && function(&c, BLOW)
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value.is_some())
        })
        .await;
        let text = status_text(&c);
        assert!(
            text.contains("Low-speed control: candidates cloud; selected cloud."),
            "{text}"
        );
        assert!(
            text.contains("Legacy feedback group: candidates cloud; selected read cloud;"),
            "{text}"
        );
    }));
}
