use super::*;
use crate::device::{Command, CommandError};
use std::sync::atomic::Ordering;

#[test]
fn a_rejected_off_preserves_the_running_low_speed() {
    let (rig, listener, cloud) = rig();
    rig.cloud
        .lock()
        .unwrap()
        .rpc_results
        .insert("get_prop".into(), json!(["coolwind", 13, 22]));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true) && fan.speed.value == Some(1))
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;
            let binding = (DID.into(), 3, 2);
            c.cloud
                .lock()
                .unwrap()
                .property_codes
                .insert(binding.clone(), -704);
            assert!(matches!(
                blow.execute(Command::Off).await,
                Err(CommandError::Rejected(_))
            ));
            c.cloud.lock().unwrap().property_codes.remove(&binding);
            blow.execute(Command::On).await.unwrap();
            let state = c.cloud.lock().unwrap();
            assert_eq!(state.numeric_writes.len(), 1);
            assert_eq!(state.numeric_writes[0].0[0]["value"], json!(false));
        })
        .await;
    }));
}

#[test]
fn a_rejected_speed_change_does_not_fail_an_existing_power_on() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true) && fan.speed.value == Some(2))
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;
            c.cloud.lock().unwrap().action_code = -704;
            let low = blow.begin_fan(FanCommand::Speed(1)).unwrap();
            assert!(matches!(
                low.accepted().await,
                Err(CommandError::Rejected(_))
            ));
            blow.execute(Command::On).await.unwrap();
            assert!(c.cloud.lock().unwrap().numeric_writes.is_empty());
        })
        .await;
    }));
}

#[test]
fn a_cancelled_queued_low_start_also_cancels_its_following_on() {
    let (rig, listener, cloud) = rig();
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert((DID.into(), 3, 2), json!(false));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| blow.snapshot().fan().unwrap().power.value == Some(false)).await;
            let low = blow.begin_fan(FanCommand::Speed(1)).unwrap();
            let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
            let off = blow.begin_fan(FanCommand::Power(false)).unwrap();
            assert_eq!(low.accepted().await, Err(CommandError::Superseded));
            assert_eq!(on.accepted().await, Err(CommandError::Superseded));
            off.accepted().await.unwrap();
            let state = c.cloud.lock().unwrap();
            assert!(state.action_requests.is_empty());
            assert_eq!(state.numeric_writes.len(), 1);
            assert_eq!(state.numeric_writes[0].0[0]["value"], json!(false));
        })
        .await;
    }));
}

#[test]
fn on_uses_new_power_feedback_after_the_heater_interlock_or_an_uncertain_off() {
    for heater_interlock in [true, false] {
        let (rig, listener, cloud) = rig();
        rig.cloud
            .lock()
            .unwrap()
            .rpc_results
            .insert("get_prop".into(), json!(["coolwind", 13, 22]));
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                let blow = function(&c, BLOW);
                eventually(|| {
                    blow.snapshot().fan().is_some_and(|fan| {
                        fan.power.value == Some(true) && fan.speed.value == Some(1)
                    })
                })
                .await;
                quiesce(&c).await;
                flush_all_polls(&c).await;
                let (release, gate) = flume::bounded(1);
                let binding = (DID.into(), 3, 2);
                {
                    let mut state = c.cloud.lock().unwrap();
                    state.read_gates.insert(binding.clone(), gate);
                    state
                        .raw_values
                        .insert(binding.clone(), json!(!heater_interlock));
                }
                if heater_interlock {
                    function(&c, HEATER).execute(Command::On).await.unwrap();
                } else {
                    c.cloud.lock().unwrap().property_status = 400;
                    assert!(blow.execute(Command::Off).await.is_err());
                    c.cloud.lock().unwrap().property_status = 200;
                }
                let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
                let pending = future::or(async { Some(on.accepted().await) }, async {
                    async_io::Timer::after(Duration::from_millis(50)).await;
                    None
                })
                .await;
                assert_eq!(pending, None);
                assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
                c.cloud.lock().unwrap().read_gates.remove(&binding);
                drop(release);
                c.clock.automatic.store(true, Ordering::Relaxed);
                on.accepted().await.unwrap();
                let writes = &c.cloud.lock().unwrap().numeric_writes;
                if heater_interlock {
                    assert_eq!(writes.len(), 2);
                    assert_eq!(
                        writes[1].0,
                        [json!({"did":DID,"siid":3,"piid":2,"value":true})]
                    );
                } else {
                    assert_eq!(writes.len(), 1);
                }
            })
            .await;
        }));
    }
}
