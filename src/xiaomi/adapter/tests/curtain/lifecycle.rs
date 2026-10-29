use super::*;

#[test]
fn curtain_rejection_and_timeout_are_reported_without_replay() {
    use crate::device::{CommandError, CurtainCommand};
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            settled(control).await;
            let function = function(&control.context.registry, "one", "curtain:2");
            let before = function.snapshot().curtain();
            peer.state.borrow_mut().hold_writes = true;
            let rejected = function
                .begin_curtain(CurtainCommand::Target(11.0))
                .unwrap();
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":-704}]}),
            )
            .await;
            assert!(matches!(
                rejected.accepted().await,
                Err(CommandError::Rejected(_))
            ));
            let timed = function.begin_curtain(CurtainCommand::Stop).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            assert_eq!(timed.accepted().await, Err(CommandError::Timeout));
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
            assert_eq!(control.cloud.lock().unwrap().numeric_writes.len(), 0);
            assert_eq!(function.snapshot().curtain(), before);
            let last = control.context.diagnostics().devices[0].commands[0]
                .last
                .clone()
                .unwrap();
            assert_eq!(last.result, Some(Err(CommandError::Timeout)));
            assert_eq!(last.actual_path, Some(PathKind::Local));
        })
        .await;
    }));
}

#[test]
fn retired_curtain_cancels_queued_work_and_old_receipt_cannot_overtake_replacement() {
    use crate::device::{CommandError, CurtainCommand};
    let mut rig = rig();
    rig.source.request_timeout = Duration::from_secs(2);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            settled(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let old_device = control
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap();
            let old = old_device.function("curtain:2").unwrap();
            let sent = old.begin_curtain(CurtainCommand::Target(15.0)).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let stale = old.begin_curtain(CurtainCommand::Stop).unwrap();
            let mut changed: Value = instance();
            changed["services"][1]["properties"][3]["value-range"] = json!([0, 100, 5]);
            let candidate = crate::xiaomi::capability::tests::source::SourceFixture::new(
                "one",
                "xiaomi.curtain.acn010",
                changed,
            );
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .candidate = Some(candidate.into());
            control.context.reconcile("one").unwrap();
            let replacement = loop {
                let current = control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .unwrap();
                if !old_device.same_instance(&current) {
                    break current;
                }
                future::yield_now().await;
            };
            let current = replacement.function("curtain:2").unwrap();
            until(|| current.snapshot().availability == Availability::Available).await;
            assert!(matches!(
                stale.accepted().await,
                Err(CommandError::Cancelled) | Err(CommandError::Unavailable(_))
            ));
            let next = current.begin_curtain(CurtainCommand::Target(77.0)).unwrap();
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":0}]}),
            )
            .await;
            sent.accepted().await.unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            let actual = peer
                .state
                .borrow()
                .seen
                .iter()
                .rfind(|(op, value)| op == "rpcReq" && value["did"] == "one")
                .unwrap()
                .1["rpc"]["params"][0]
                .clone();
            assert_eq!(actual["piid"], 4);
            assert_eq!(actual["value"], 75);
            let observed = current.snapshot().curtain();
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":0}]}),
            )
            .await;
            next.accepted().await.unwrap();
            assert_eq!(current.snapshot().curtain(), observed);
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
        })
        .await;
    }));
}

#[test]
fn curtain_cloud_failure_and_timeout_do_not_retry_or_infer_motion() {
    use crate::device::{CommandError, CurtainCommand};
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            let function = function(&c.context.registry, "one", "curtain:2");
            let before = function.snapshot().curtain();
            c.cloud.lock().unwrap().property_code = -704;
            let rejected = function
                .begin_curtain(CurtainCommand::Target(44.0))
                .unwrap();
            assert!(matches!(
                rejected.accepted().await,
                Err(CommandError::Rejected(_))
            ));
            let (_release, gate) = flume::bounded(1);
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.property_code = 0;
                cloud.write_gates.insert("one".into(), gate);
            }
            let timed = function.begin_curtain(CurtainCommand::Stop).unwrap();
            assert_eq!(timed.accepted().await, Err(CommandError::Timeout));
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 2);
            assert_eq!(function.snapshot().curtain(), before);
            let last = c.context.diagnostics().devices[0].commands[0]
                .last
                .clone()
                .unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Cloud));
            assert_eq!(last.result, Some(Err(CommandError::Timeout)));
        })
        .await;
    }));
}
