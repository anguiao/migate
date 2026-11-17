use super::*;
use crate::xiaomi::capability::PropertyBinding;

const BRIGHTNESS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
const TEMPERATURE: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };

fn pending<'a>(status: &'a AdapterStatus, did: &str) -> &'a PendingDevice {
    let entries = status
        .pending
        .iter()
        .filter(|entry| entry.did == did)
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "{status:?}");
    entries[0]
}

fn assert_reasons(status: &AdapterStatus, did: &str, reasons: &[&str]) {
    let entry = pending(status, did);
    let positions = reasons
        .iter()
        .map(|reason| {
            entry
                .reason
                .find(reason)
                .unwrap_or_else(|| panic!("missing {reason:?}: {}", entry.reason))
        })
        .collect::<Vec<_>>();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(entry.reason.split("; ").count(), reasons.len(), "{entry:?}");
}

fn version_exhausted_rig() -> Rig {
    let mut rig = Rig::new(true, &["one"]);
    let mut configuration = rig.store.devices().load_configuration().unwrap();
    configuration.devices[0].configuration_version = u32::MAX;
    configuration.devices[0].label = "Saved outlet name".into();
    rig.store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
    let (source, ready) = rig.reopen_adapter();
    rig.source = source;
    rig.ready = ready;
    rig
}

#[test]
fn pending_description_subscriptions_and_reads_recover_independently() {
    let rig = Rig::light("xiaomi.light.ceil04");
    let statuses = Rc::new(RefCell::new(Vec::<AdapterStatus>::new()));
    let reported = statuses.clone();
    block_on(rig.run_with_status(
        move |status| reported.borrow_mut().push(status),
        |c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            peer.value("one", BRIGHTNESS, json!(50));
            peer.value("one", TEMPERATURE, json!(4000));
            {
                let mut state = peer.state.borrow_mut();
                for binding in [BRIGHTNESS, TEMPERATURE] {
                    state
                        .sub_reject
                        .insert(("one".into(), binding.siid, binding.piid));
                    state
                        .read_fail
                        .insert(("one".into(), binding.siid, binding.piid));
                }
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| {
                    peer.count("subscribe", "one", 2) >= 9 && peer.count("get", "one", 2) >= 9
                })
                .await;
                let work = control.context.directory.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                until(|| {
                    work.local_exhausted()
                        && !work.refresh.borrow().contains("light:2", BRIGHTNESS)
                        && !work.refresh.borrow().contains("light:2", TEMPERATURE)
                })
                .await;
                let urn = "urn:miot-spec-v2:device:light:0000A001:xiaomi-ceil04:2:0000C802";
                {
                    let mut cloud = control.cloud.lock().unwrap();
                    cloud.instance_types.insert("one".into(), urn.into());
                    cloud.instance_status.insert(urn.into(), 503);
                }
                control.context.request_sync();
                until(|| {
                    control.context.directory.borrow().devices["one"]
                        .description_problem()
                        .as_ref()
                        .is_some_and(|reason| reason.contains("503"))
                })
                .await;
                assert_reasons(
                    &control.context.diagnostics(),
                    "one",
                    &[
                        "503",
                        "one subscribe property 2.2",
                        "one subscribe property 2.3",
                        "one read 2.2",
                        "one read 2.3",
                    ],
                );
                let status_changed = control.context.status_changed.listen();
                let changed = control.context.changed.listen();
                let before = control.context.pending_devices();
                assert_eq!(control.context.pending_devices(), before);
                assert_eq!(control.context.pending_devices(), before);
                assert!(poll_once(status_changed).await.is_none());
                assert!(poll_once(changed).await.is_none());
                until(|| {
                    statuses.borrow().last().is_some_and(|status| {
                        status.pending.iter().any(|entry| {
                            entry.did == "one"
                                && entry.reason.contains("503")
                                && entry.reason.contains("one read 2.3")
                        })
                    })
                })
                .await;

                peer.state
                    .borrow_mut()
                    .sub_reject
                    .remove(&("one".into(), 2, 2));
                peer.list_changed(&["one"]);
                until(|| {
                    statuses.borrow().last().is_some_and(|status| {
                        status.pending.iter().any(|entry| {
                            entry.did == "one"
                                && !entry.reason.contains("subscribe property 2.2")
                                && entry.reason.contains("subscribe property 2.3")
                        })
                    })
                })
                .await;
                assert_reasons(
                    statuses.borrow().last().unwrap(),
                    "one",
                    &[
                        "503",
                        "one subscribe property 2.3",
                        "one read 2.2",
                        "one read 2.3",
                    ],
                );

                peer.state
                    .borrow_mut()
                    .read_fail
                    .remove(&("one".into(), 2, 2));
                work.refresh_property("light:2", BRIGHTNESS);
                until(|| {
                    statuses.borrow().last().is_some_and(|status| {
                        status.pending.iter().any(|entry| {
                            entry.did == "one" && !entry.reason.contains("one read 2.2")
                        })
                    })
                })
                .await;
                assert_reasons(
                    statuses.borrow().last().unwrap(),
                    "one",
                    &["503", "one subscribe property 2.3", "one read 2.3"],
                );

                peer.state
                    .borrow_mut()
                    .read_fail
                    .remove(&("one".into(), 2, 3));
                work.refresh_property("light:2", TEMPERATURE);
                until(|| {
                    statuses.borrow().last().is_some_and(|status| {
                        status.pending.iter().any(|entry| {
                            entry.did == "one" && !entry.reason.contains("one read 2.3")
                        })
                    })
                })
                .await;
                assert_reasons(
                    statuses.borrow().last().unwrap(),
                    "one",
                    &["503", "one subscribe property 2.3"],
                );

                peer.state.borrow_mut().sub_reject.clear();
                peer.list_changed(&["one"]);
                until(|| {
                    statuses.borrow().last().is_some_and(|status| {
                        status
                            .pending
                            .iter()
                            .any(|entry| entry.did == "one" && !entry.reason.contains("subscribe"))
                    })
                })
                .await;
                assert_reasons(statuses.borrow().last().unwrap(), "one", &["503"]);

                let catalogs = control.cloud.lock().unwrap().catalogs;
                control.context.request_sync();
                until(|| control.cloud.lock().unwrap().catalogs > catalogs).await;
                assert_reasons(&control.context.diagnostics(), "one", &["503"]);

                control.cloud.lock().unwrap().instance_types.remove("one");
                control.context.request_sync();
                until(|| {
                    statuses
                        .borrow()
                        .last()
                        .is_some_and(|status| status.pending.iter().all(|entry| entry.did != "one"))
                })
                .await;
                assert!(Rc::ptr_eq(
                    &work,
                    control.context.directory.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                ));
            })
            .await;
        },
    ));
}

#[test]
fn pending_first_description_keeps_the_confirmed_home_catalog_name() {
    let rig = Rig::new(false, &["one"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .names
            .insert("one".into(), "Living room outlet".into());
        cloud.detail_status = 503;
        cloud.catalog_groups.push(json!({
            "did":"foreign", "name":"Other home's outlet", "model":"cuco.plug.v3",
            "spec_type":rig.data[0].instance_type(), "isOnline":true
        }));
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .directory
                .borrow()
                .devices
                .get("one")
                .is_some_and(|slot| {
                    slot.description_problem()
                        .as_ref()
                        .is_some_and(|reason| reason.contains("503"))
                })
        })
        .await;
        assert!(c.publication.current().configuration.devices.is_empty());
        let status = c.context.diagnostics();
        assert_eq!(pending(&status, "one").name, "Living room outlet");
        assert_eq!(status.pending.len(), 1);
        assert!(!c.context.directory.borrow().devices.contains_key("foreign"));
    }));
}

#[test]
fn pending_first_plan_keeps_the_description_name_without_an_installed_instance() {
    let mut rig = Rig::new(true, &["one"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("multi".into());
        cloud
            .names
            .insert("multi".into(), "Study power strip".into());
    }
    let mut configuration = rig.store.devices().load_configuration().unwrap();
    let historical = super::support::device_with_key(rig.data[0].device(), "history");
    configuration
        .devices
        .push(crate::storage::tests::fixtures::identity(
            &historical.physical_description("History"),
            65533,
        ));
    rig.store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
    let (source, ready) = rig.reopen_adapter();
    rig.source = source;
    rig.ready = ready;
    block_on(rig.run(|c| async move {
        until(|| c.publication.failure().is_some()).await;
        let status = c.context.diagnostics();
        let entry = pending(&status, "multi");
        assert_eq!(entry.name, "Study power strip");
        assert!(entry.reason.contains("exhausted"));
        assert!(
            c.publication
                .current()
                .configuration
                .device(&DeviceKey::new("xiaomi", "multi"))
                .is_none()
        );
        assert!(
            c.context.directory.borrow().devices["multi"]
                .admitted
                .is_none()
        );
    }));
}

#[test]
fn pending_successful_unchanged_planning_clears_only_its_previous_failure() {
    let rig = Rig::new(true, &["one"]);
    let source = rig.source.source_handle();
    let app = crate::app::App::open(source.clone(), rig.store.clone()).unwrap();
    rig.source
        .context
        .pending_description("one", "description read failed".into());
    source.pending(
        &DeviceKey::new("xiaomi", "one"),
        &app.current().configuration,
        "configuration planning failed".into(),
    );
    assert_reasons(
        &rig.source.context.diagnostics(),
        "one",
        &["description read failed", "configuration planning failed"],
    );
    assert!(app.plan().unwrap().is_none());
    assert_reasons(
        &rig.source.context.diagnostics(),
        "one",
        &["description read failed"],
    );
    let description = rig.source.context.directory.borrow().devices["one"]
        .latest_description
        .clone()
        .unwrap();
    rig.source
        .context
        .directory
        .borrow_mut()
        .accept_description("one", description);
    assert!(rig.source.context.diagnostics().pending.is_empty());
}

#[test]
fn pending_removed_device_version_overflow_survives_without_a_slot_until_readmission() {
    let rig = version_exhausted_rig();
    rig.source
        .context
        .pending_description("one", "old description failure".into());
    rig.cloud.lock().unwrap().members.clear();
    block_on(rig.run_result_without_publication(|c| async move {
        until(|| !c.context.directory.borrow().devices.contains_key("one")).await;
        let key = DeviceKey::new("xiaomi", "one");
        let expected = "Device configuration version is exhausted for one";
        for _ in 0..2 {
            let changed = c.context.status_changed.listen();
            assert!(
                c.publication
                    .plan()
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
            assert!(poll_once(changed).await.is_some());
            let status = c.context.diagnostics();
            assert_eq!(pending(&status, "one").name, "Saved outlet name");
            assert_reasons(&status, "one", &[expected]);
            assert!(status.devices.is_empty());
            assert!(!c.context.directory.borrow().devices.contains_key("one"));
            assert!(c.context.registry.get(&key).is_none());
        }

        let synchronized = c.context.configuration_changed.listen();
        c.context.request_sync();
        synchronized.await;
        assert_reasons(&c.context.diagnostics(), "one", &[expected]);
        assert!(!c.context.directory.borrow().devices.contains_key("one"));

        c.cloud.lock().unwrap().members.push("one".into());
        c.context.request_sync();
        until(|| {
            c.context
                .directory
                .borrow()
                .devices
                .get("one")
                .is_some_and(|slot| {
                    slot.confirmed && slot.latest_description.is_some() && slot.admitted.is_none()
                })
        })
        .await;
        assert_reasons(&c.context.diagnostics(), "one", &[expected]);
        let changed = c.context.status_changed.listen();
        let plan = c.publication.plan().unwrap().unwrap();
        assert!(poll_once(changed).await.is_some());
        assert!(c.context.diagnostics().pending.is_empty());
        c.publication.apply(c.publication.prepare(plan)).unwrap();
        assert!(c.context.diagnostics().pending.is_empty());
        assert!(c.context.registry.get(&key).is_some());
        assert_eq!(
            c.publication
                .current()
                .configuration
                .device(&key)
                .unwrap()
                .configuration_version,
            u32::MAX
        );
    }))
    .unwrap();
}

#[test]
fn pending_version_overflow_does_not_disclose_the_previous_home_name() {
    let rig = version_exhausted_rig();
    let source = rig.source.source_handle();
    let app = crate::app::App::open(source, rig.store.clone()).unwrap();
    rig.source.context.remove("one").unwrap();
    assert!(app.plan().is_err());
    assert_eq!(
        pending(&rig.source.context.diagnostics(), "one").name,
        "Saved outlet name"
    );
    rig.source.context.directory.borrow_mut().select_home(
        Scope {
            uid: rig.uid.clone(),
            home_id: "other".into(),
        },
        Vec::new(),
        &BTreeSet::new(),
    );
    assert!(rig.source.context.diagnostics().pending.is_empty());
    assert!(app.plan().is_err());
    let status = rig.source.context.diagnostics();
    assert_eq!(pending(&status, "one").name, "one");
    assert_reasons(
        &status,
        "one",
        &["Device configuration version is exhausted for one"],
    );
    assert!(status.devices.is_empty());
    assert!(rig.source.context.directory.borrow().devices.is_empty());
}
