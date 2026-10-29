use super::*;
use crate::device::{CommandError, FanCommand};
use futures_lite::future::poll_once;

#[test]
fn retired_instance_cannot_resume_a_sent_speed_or_affect_its_replacement() {
    let mut rig = rig();
    rig.source.request_timeout = Duration::from_secs(2);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).power.value == Some(true)
                    && fan(control).speed.value == Some(40)
                    && work(control).fan_mode_observation("fan:2").value == Some(1)
                    && work(control).function_observer("fan:2").is_some()
            })
            .await;
            let key = DeviceKey::new("xiaomi", "one");
            let old_device = control.context.registry.get(&key).unwrap();
            let old = old_device.function("fan:2").unwrap();
            peer.state.borrow_mut().hold_writes = true;
            let sent = old.begin_fan(FanCommand::Speed(60)).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let queued = old.begin_fan(FanCommand::Swing(true)).unwrap();
            let original = work(control).native.clone();
            let original_function = &original.device().functions()[0];
            let renamed = crate::xiaomi::capability::ParsedFunction::fan_control(
                original_function.id(),
                "Updated fan",
                original_function.fan().unwrap().clone(),
            );
            let parsed =
                super::super::support::device_with_functions(original.device(), vec![renamed]);
            let native = crate::xiaomi::capability::NativeDescription::new(
                original.model().into(),
                original.instance_type().into(),
                parsed,
            )
            .unwrap();
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .candidate
                .as_mut()
                .unwrap()
                .native = Some(native);
            control.context.reconcile("one").unwrap();
            until(|| {
                control
                    .context
                    .registry
                    .get(&key)
                    .is_some_and(|current| !old_device.same_instance(&current))
            })
            .await;
            let replacement = control
                .context
                .registry
                .get(&key)
                .unwrap()
                .function("fan:2")
                .unwrap();
            until(|| {
                replacement.snapshot().availability == Availability::Available
                    && replacement.snapshot().fan().unwrap().power.value == Some(true)
            })
            .await;
            assert!(matches!(
                queued.accepted().await,
                Err(CommandError::Cancelled) | Err(CommandError::Unavailable(_))
            ));
            let next = replacement.begin_fan(FanCommand::Speed(72)).unwrap();
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            assert!(poll_once(next.accepted()).await.is_none());
            peer.release_property(
                "one",
                MODE,
                json!({"result":[{"did":"one","siid":2,"piid":3,"code":0}]}),
            )
            .await;
            assert!(matches!(
                sent.accepted().await,
                Err(CommandError::Cancelled) | Err(CommandError::Unavailable(_))
            ));
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            let observed = replacement.snapshot().fan();
            peer.release_property(
                "one",
                MODE,
                json!({"result":[{"did":"one","siid":2,"piid":3,"code":0}]}),
            )
            .await;
            until(|| peer.count("rpcReq", "one", 0) == 3).await;
            peer.release_property(
                "one",
                SPEED,
                json!({"result":[{"did":"one","siid":8,"piid":1,"code":0}]}),
            )
            .await;
            next.accepted().await.unwrap();
            assert_eq!(replacement.snapshot().fan(), observed);
            assert_eq!(peer.count("rpcReq", "one", 0), 3);
        })
        .await;
    }));
}
