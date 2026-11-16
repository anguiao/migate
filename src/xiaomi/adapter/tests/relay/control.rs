use super::*;

#[test]
fn unpublished_relays_are_read_and_controlled_per_channel_without_optimism() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_some_and(|device| {
                        device.availability() == Availability::Available
                            && [2, 3, 4].iter().all(|siid| {
                                device.function(&format!("relay:{siid}")).is_some_and(|f| {
                                    f.snapshot().power() == Some(Power::Off)
                                        && f.snapshot().relay_mode() == Some(RelayMode::Wireless)
                                })
                            })
                    })
            })
            .await;
            let key = DeviceKey::new("xiaomi", "one");
            assert!(!control.context.graph.borrow().published.contains(&key));
            let one = function(&control.context.registry, "one", "relay:2");
            let two = function(&control.context.registry, "one", "relay:3");
            let three = function(&control.context.registry, "one", "relay:4");
            until(|| {
                control.context.directory.borrow().devices["one"]
                    .admitted
                    .as_ref()
                    .unwrap()
                    .function_observer("relay:3")
                    .is_some()
            })
            .await;
            let mut changes = two.subscribe();
            let terminal = control.terminal();
            let result = terminal
                .handle_line("on one relay:3")
                .await
                .unwrap()
                .unwrap();
            assert!(result.contains("command accepted"), "{result}");
            assert_eq!(two.snapshot().power(), Some(Power::Off));
            assert_eq!(peer.count("rpcReq", "one", 3), 1);
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
            assert_eq!(peer.count("rpcReq", "one", 4), 0);
            assert!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .any(|(operation, value)| operation == "rpcReq"
                        && value["did"] == "one"
                        && value["rpc"]["params"][0]["siid"] == 3
                        && value["rpc"]["params"][0]["piid"] == 1
                        && value["rpc"]["params"][0]["value"] == true)
            );
            peer.notify_value("one", binding(3, 1), json!(true), false);
            assert_eq!(changes.changed().await.power(), Some(Power::On));
            assert_eq!(one.snapshot().power(), Some(Power::Off));
            assert_eq!(three.snapshot().power(), Some(Power::Off));
            let result = terminal
                .handle_line("off one relay:3")
                .await
                .unwrap()
                .unwrap();
            assert!(result.contains("command accepted"), "{result}");
            assert_eq!(two.snapshot().power(), Some(Power::On));
            assert_eq!(peer.count("rpcReq", "one", 3), 2);
        })
        .await;
    }));
}

#[test]
fn cloud_relay_reads_writes_and_notifications_use_parent_did_and_current_values() {
    let (rig, listener, peer) = with_cloud_rig(rig());
    {
        let mut cloud = rig.cloud.lock().unwrap();
        for siid in [2, 3, 4] {
            cloud
                .raw_values
                .insert(("one".into(), siid, 1), json!(false));
            cloud.raw_values.insert(("one".into(), siid, 2), json!(1));
        }
    }
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            let relay = function(&c.context.registry, "one", "relay:4");
            until(|| {
                relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
                    && relay.snapshot().power() == Some(Power::Off)
                    && c.context.directory.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .function_observer("relay:4")
                        .is_some()
            })
            .await;
            relay.execute(Command::On).await.unwrap();
            assert_eq!(relay.snapshot().power(), Some(Power::Off));
            assert!(c.cloud.lock().unwrap().property_writes.iter().any(
                |(did, siid, piid, value, _)| did == "one" && *siid == 4 && *piid == 1 && *value
            ));
            peer.property_at("one", 4, 1, json!(true));
            until(|| relay.snapshot().power() == Some(Power::On)).await;
            peer.property_at("one", 4, 2, json!(0));
            until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wired)).await;
            assert!(
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .any(|(did, siid, piid)| did == "one" && *siid == 4 && *piid == 2)
            );
            let work = c.context.directory.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            assert_eq!(work.observation_source("relay:4"), Some(PathKind::Cloud));
            assert_eq!(
                work.property_source("relay:4", binding(4, 1)),
                Some(PathKind::Cloud)
            );
            let old_source = work.function_observer("relay:4").unwrap();
            c.context
                .directory
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .cloud_forbidden = true;
            c.context.update_device_availability("one");
            c.context.changed.notify(usize::MAX);
            c.context.property_value_event(
                &old_source,
                "one",
                binding(4, 1),
                &json!(false),
                None,
                false,
            );
            c.context.property_value_event(
                &old_source,
                "one",
                binding(4, 2),
                &json!(1),
                None,
                false,
            );
            assert_eq!(relay.snapshot().power(), Some(Power::On));
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));
            assert!(relay.execute(Command::Off).await.is_err());
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        })
        .await;
    }));
}
