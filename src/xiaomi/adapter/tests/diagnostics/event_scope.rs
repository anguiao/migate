use super::*;
use crate::{
    configuration::OperationKind,
    xiaomi::cloud::{CloudError, CloudErrorKind},
};

fn pending_offline(c: &Control, did: &str) {
    c.context
        .directory
        .borrow_mut()
        .devices
        .get_mut(did)
        .unwrap()
        .cloud_access
        .as_mut()
        .unwrap()
        .online = false;
}

async fn cloud_ready(c: &Control, dids: &[&str]) {
    until(|| {
        dids.iter().all(|did| {
            c.context.cloud_candidate(did, true).is_ok()
                && c.context
                    .registry
                    .get(&DeviceKey::new("xiaomi", *did))
                    .unwrap()
                    .functions()[0]
                    .snapshot()
                    .power()
                    == Some(Power::Off)
        })
    })
    .await;
    c.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
}

fn assert_pending_history(c: &Control, did: &str, before: &Option<String>) {
    assert_eq!(&recorded(c, did), before);
    let status = c.context.diagnostics();
    assert_eq!(
        status
            .devices
            .iter()
            .find(|device| device.did == did)
            .unwrap()
            .selected_control,
        None
    );
    assert_eq!(&recorded(c, did), before);
}

fn confirm_group_members(c: &Control, members: &[&str]) {
    use crate::xiaomi::mijia::{
        CloudCatalogDevice, CloudDeviceCatalog, CloudGroupMembership, CloudGroupStatus,
    };

    let mut directory = c.context.directory.borrow_mut();
    let group = directory
        .confirmed_groups()
        .into_iter()
        .find(|group| group.membership.group_did == "group-1")
        .unwrap();
    let catalog = CloudDeviceCatalog {
        devices: vec![CloudCatalogDevice {
            did: group.record_did,
            name: group.name,
            model: group.model,
            instance_type: group.instance_type,
            online: group.online,
            group: CloudGroupStatus::Confirmed(CloudGroupMembership {
                group_did: group.membership.group_did,
                is_physical: group.membership.is_physical,
                members: members.iter().map(|did| (*did).to_owned()).collect(),
            }),
            split: None,
        }],
    };
    let scope = directory.scope.clone().unwrap();
    directory.confirm_catalog(
        &catalog,
        &scope,
        &["member-a".into(), "member-b".into()].into(),
    );
}

#[test]
fn cloud_write_device_errors_leave_other_devices_pending_until_their_own_event() {
    for code in [-704_042_011, -704_010_000, -704_012_904, -704_012_905] {
        let rig = Rig::new(true, &["one", "two"]);
        rig.cloud
            .lock()
            .unwrap()
            .online
            .extend(["one".into(), "two".into()]);
        block_on(rig.run(|c| async move {
            cloud_ready(&c, &["one", "two"]).await;
            let other = recorded(&c, "two");
            let global = c.context.diagnostics().routing.last_switch_reason;
            pending_offline(&c, "two");
            c.cloud.lock().unwrap().property_code = code;
            assert!(matches!(
                function(&c.context.registry, "one", "outlet:2")
                    .execute(Command::On)
                    .await,
                Err(crate::device::CommandError::Rejected(_))
            ));
            assert!(recorded(&c, "one").unwrap().contains(&code.to_string()));
            assert_pending_history(&c, "two", &other);
            assert_eq!(c.context.diagnostics().routing.last_switch_reason, global);

            let path = c.context.mijia_path().unwrap();
            let error = CloudError::new(
                "set properties",
                CloudErrorKind::PropertyRejected(-704_010_000),
            );
            c.context.cloud_error(&path, Some("two"), &error);
            assert!(recorded(&c, "two").unwrap().contains("-704010000"));
        }));
    }
}

#[test]
fn ordinary_and_repeated_cloud_rejections_do_not_process_pending_access_changes() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one", "two"]).await;
        let path = c.context.mijia_path().unwrap();
        let one = recorded(&c, "one");
        let other = recorded(&c, "two");
        pending_offline(&c, "one");
        pending_offline(&c, "two");
        c.context.cloud_error(
            &path,
            Some("one"),
            &CloudError::new("set properties", CloudErrorKind::PropertyRejected(-9)),
        );
        assert_pending_history(&c, "one", &one);
        assert_pending_history(&c, "two", &other);

        let error = CloudError::new(
            "set properties",
            CloudErrorKind::PropertyRejected(-704_010_000),
        );
        c.context.cloud_error(&path, Some("one"), &error);
        let rejected = recorded(&c, "one");
        c.context.cloud_error(&path, Some("one"), &error);
        assert_eq!(recorded(&c, "one"), rejected);
        assert_pending_history(&c, "two", &other);
    }));
}

#[test]
fn first_http_pause_processes_every_device_and_preserves_the_first_backoff() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one", "two"]).await;
        c.cloud.lock().unwrap().property_status = 503;
        assert!(
            function(&c.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .is_err()
        );
        let deadline = c.context.directory.borrow().sync_deadline();
        assert_eq!(deadline, Some(c.clock.now() + Duration::from_secs(1)));
        for did in ["one", "two"] {
            assert!(recorded(&c, did).unwrap().contains("HTTP status 503"));
            assert!(c.context.cloud_candidate(did, true).is_err());
        }
        c.context.cloud_error(
            &c.context.mijia_path().unwrap(),
            Some("one"),
            &CloudError::new("set properties", CloudErrorKind::HttpStatus(503)),
        );
        assert_eq!(c.context.directory.borrow().sync_deadline(), deadline);
    }));
}

#[test]
fn repeated_http_pause_does_not_process_an_undispatched_hub_access_change() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            until(|| control.context.directory.borrow().scope_valid()).await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let path = control.context.mijia_path().unwrap();
            let error = CloudError::new("set properties", CloudErrorKind::HttpStatus(503));
            control.context.cloud_error(&path, Some("one"), &error);
            let other = recorded(control, "two");
            let deadline = control.context.directory.borrow().sync_deadline();
            control.context.hub.borrow_mut().device_offline("two");
            control.context.cloud_error(&path, Some("one"), &error);
            assert_pending_history(control, "two", &other);
            assert_eq!(control.context.directory.borrow().sync_deadline(), deadline);
            let handle = control
                .context
                .hub
                .borrow()
                .session()
                .unwrap()
                .handle
                .clone();
            local_device_offline(&control.context, &handle, "two");
            assert_eq!(
                recorded(control, "two").as_deref(),
                Some("hub reported device two offline or missing")
            );
        })
        .await;
    }));
}

#[test]
fn cloud_read_access_rejection_processes_dependent_virtual_groups_only() {
    let (rig, listener, cloud) = with_cloud_rig(repair_rig(0, true));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            repair_ready(&c).await;
            until(|| c.context.active_notification("group-1") == Some(PathKind::Cloud)).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let other = recorded(&c, "member-b");
            pending_offline(&c, "member-b");
            let path = c.context.mijia_path().unwrap();
            let work = c.context.directory.borrow().devices["member-a"]
                .admitted
                .clone()
                .unwrap();
            c.context.cloud_property_read_error(
                &path,
                &work,
                &CloudError::new(
                    "read properties",
                    CloudErrorKind::PropertyRejected(-704_010_000),
                ),
            );
            assert_eq!(c.context.active_notification("group-1"), None);
            assert!(recorded(&c, "group-1").unwrap().contains("-704010000"));
            assert_pending_history(&c, "member-b", &other);
        })
        .await;
    }));
}

#[test]
fn metadata_and_native_installations_only_process_native_routing_changes() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one", "two"]).await;
        let one = recorded(&c, "one");
        let other = recorded(&c, "two");
        let before = c.publication.current().configuration.clone();
        {
            let mut directory = c.context.directory.borrow_mut();
            directory
                .devices
                .get_mut("one")
                .unwrap()
                .latest_description
                .as_mut()
                .unwrap()
                .raw
                .name = Some("Updated catalog label".into());
        }
        pending_offline(&c, "one");
        pending_offline(&c, "two");
        let plan = c.publication.plan().unwrap().unwrap();
        assert!(
            plan.operations
                .iter()
                .all(|operation| operation.kind == OperationKind::Reused)
        );
        c.publication.apply(c.publication.prepare(plan)).unwrap();
        assert_pending_history(&c, "one", &one);
        assert_pending_history(&c, "two", &other);

        let mut changed = SourceFixture::committed(&before, "one");
        let mut extra = changed.raw.instance["services"][1].clone();
        extra["iid"] = json!(42);
        changed.raw.instance["services"]
            .as_array_mut()
            .unwrap()
            .push(extra);
        c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut("one")
            .unwrap()
            .latest_description = Some(changed.raw.recognize(false).unwrap());
        let plan = c.publication.plan().unwrap().unwrap();
        assert!(
            plan.operations
                .iter()
                .any(|operation| operation.key.id == "one"
                    && operation.kind == OperationKind::ReplacedNative)
        );
        c.publication.apply(c.publication.prepare(plan)).unwrap();
        assert_eq!(
            recorded(&c, "one").as_deref(),
            Some("committed configuration installed")
        );
        assert_pending_history(&c, "two", &other);
    }));
}

#[test]
fn logical_only_physical_group_replacement_preserves_pending_group_history() {
    let rig = repair_rig(0, true);
    rig.cloud.lock().unwrap().catalog_groups[0]["extra"]["member_list"]["is_physical"] =
        json!(true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let group = recorded(&c, "group-1");
        let other = recorded(&c, "member-b");
        let work = c.context.directory.borrow().devices["group-1"]
            .admitted
            .clone()
            .unwrap();
        let mut narrowed = ls2307_instance();
        narrowed["services"][1]["properties"][2]["value-range"] = json!([3000, 6400, 1]);
        c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut("member-a")
            .unwrap()
            .latest_description =
            Some(SourceFixture::new("member-a", "devcea.light.ls2307", narrowed).into());
        pending_offline(&c, "group-1");
        pending_offline(&c, "member-b");
        let plan = c.publication.plan().unwrap().unwrap();
        assert!(
            plan.operations
                .iter()
                .any(|operation| operation.key.id == "group-1"
                    && operation.kind == OperationKind::ReplacedLogical)
        );
        assert_eq!(c.context.graph.borrow().rules, plan.derived.rules);
        c.publication.apply(c.publication.prepare(plan)).unwrap();
        assert!(Rc::ptr_eq(
            &work,
            c.context.directory.borrow().devices["group-1"]
                .admitted
                .as_ref()
                .unwrap()
        ));
        assert_pending_history(&c, "group-1", &group);
        assert_pending_history(&c, "member-b", &other);
    }));
}

#[test]
fn changed_group_membership_processes_the_group_without_consuming_old_or_new_members() {
    let (rig, listener, cloud) = with_cloud_rig(repair_rig(0, true));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            repair_ready(&c).await;
            until(|| c.context.active_notification("group-1") == Some(PathKind::Cloud)).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let path = c.context.mijia_path().unwrap();
            c.context.cloud_error(
                &path,
                Some("member-b"),
                &CloudError::new(
                    "set properties",
                    CloudErrorKind::PropertyRejected(-704_010_000),
                ),
            );
            assert_eq!(c.context.active_notification("group-1"), None);
            let other = recorded(&c, "member-b");
            c.context
                .directory
                .borrow_mut()
                .devices
                .get_mut("member-b")
                .unwrap()
                .cloud_forbidden = false;
            confirm_group_members(&c, &["member-a"]);
            let plan = c.publication.plan().unwrap().unwrap();
            assert!(
                plan.operations
                    .iter()
                    .all(|operation| operation.kind == OperationKind::Reused)
            );
            c.publication.apply(c.publication.prepare(plan)).unwrap();
            assert_eq!(
                c.context.active_notification("group-1"),
                Some(PathKind::Cloud)
            );
            assert_eq!(
                recorded(&c, "group-1").as_deref(),
                Some("committed configuration installed")
            );
            assert_eq!(recorded(&c, "member-b"), other);
            assert_eq!(
                c.context
                    .diagnostics()
                    .devices
                    .iter()
                    .find(|device| device.did == "member-b")
                    .unwrap()
                    .selected_control,
                Some(PathKind::Cloud)
            );
            assert_eq!(recorded(&c, "member-b"), other);

            let selection = c.context.directory.borrow().devices["group-1"].routing_selection;
            pending_offline(&c, "member-b");
            confirm_group_members(&c, &["member-b"]);
            let plan = c.publication.plan().unwrap().unwrap();
            c.publication.apply(c.publication.prepare(plan)).unwrap();
            assert_eq!(c.context.active_notification("group-1"), None);
            assert_ne!(
                c.context.directory.borrow().devices["group-1"].routing_selection,
                selection
            );
            assert_eq!(recorded(&c, "member-b"), other);
        })
        .await;
    }));
}

#[test]
fn removal_releases_the_baseline_without_processing_other_device_history() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one", "two"]).await;
        let other = recorded(&c, "two");
        let original = c.context.directory.borrow().devices["one"]
            .latest_description
            .clone()
            .unwrap();
        c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut("one")
            .unwrap()
            .latest_description
            .as_mut()
            .unwrap()
            .native = None;
        pending_offline(&c, "two");
        let plan = c.publication.plan().unwrap().unwrap();
        assert!(
            plan.operations
                .iter()
                .any(|operation| operation.key.id == "one"
                    && operation.kind == OperationKind::Removed)
        );
        c.publication.apply(c.publication.prepare(plan)).unwrap();
        assert!(
            c.context.directory.borrow().devices["one"]
                .routing_selection
                .is_none()
        );
        assert_eq!(recorded(&c, "one"), None);
        assert_pending_history(&c, "two", &other);

        c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut("one")
            .unwrap()
            .latest_description = Some(original);
        let plan = c.publication.plan().unwrap().unwrap();
        assert!(
            plan.operations.iter().any(
                |operation| operation.key.id == "one" && operation.kind == OperationKind::Added
            )
        );
        c.publication.apply(c.publication.prepare(plan)).unwrap();
        assert_eq!(recorded(&c, "one"), None);
        assert!(
            c.context.directory.borrow().devices["one"]
                .routing_selection
                .is_some()
        );
        assert_pending_history(&c, "two", &other);
    }));
}
