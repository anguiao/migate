use super::*;

fn mode_source(status: &AdapterStatus) -> Option<PathKind> {
    status
        .devices
        .iter()
        .find(|device| device.did == "one")?
        .functions
        .iter()
        .find(|function| function.function == "relay:2")?
        .properties
        .iter()
        .find(|property| property.role == PropertyRole::Mode)?
        .observation_source
}

#[test]
fn unchanged_relay_mode_feedback_publishes_its_new_property_source_without_queries() {
    let (rig, listener, cloud) = with_cloud_rig(Rig::profile_light(
        "zimi.switch.dhkg05",
        serde_json::from_str(include_str!(
            "../../../../../tests/fixtures/miot/zimi-dhkg05.json"
        ))
        .unwrap(),
    ));
    let reports = Rc::new(RefCell::new(Vec::<AdapterStatus>::new()));
    let published = reports.clone();
    block_on(rig.run_with_status(
        move |status| published.borrow_mut().push(status),
        |c| async move {
            future::or(cloud.run(&listener), async {
                c.announce();
                let local = Rc::new(Peer::default());
                local.state.borrow_mut().pushless.insert("one".into());
                for siid in [2, 3, 4] {
                    local.value(
                        "one",
                        crate::xiaomi::capability::PropertyBinding { siid, piid: 1 },
                        json!(false),
                    );
                    local.value(
                        "one",
                        crate::xiaomi::capability::PropertyBinding { siid, piid: 2 },
                        json!(0),
                    );
                }
                let control = &c;
                let reports = &reports;
                let cloud = &cloud;
                c.peer(local, |_peer| async move {
                    until(|| {
                        cloud_active(control, "one")
                            && reports
                                .borrow()
                                .last()
                                .is_some_and(|status| mode_source(status) == Some(PathKind::Local))
                    })
                    .await;
                    control
                        .clock
                        .automatic
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    async_io::Timer::after(Duration::from_millis(30)).await;
                    let current = control.context.directory.borrow().devices["one"]
                        .work
                        .clone()
                        .unwrap();
                    cloud.property_at("one", 2, 2, json!(0));
                    until(|| {
                        current.property_source(
                            "relay:2",
                            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 2 },
                        ) == Some(PathKind::Cloud)
                    })
                    .await;
                    async_io::Timer::after(Duration::from_millis(30)).await;
                    assert!(
                        reports
                            .borrow()
                            .iter()
                            .any(|status| mode_source(status) == Some(PathKind::Cloud))
                    );
                })
                .await;
            })
            .await;
        },
    ));
}

#[test]
fn refresh_errors_and_successful_recovery_are_published_without_queries() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let published = Rc::new(RefCell::new(Vec::<AdapterStatus>::new()));
    let reports = published.clone();
    block_on(rig.run_with_status(
        move |status| reports.borrow_mut().push(status),
        |c| async move {
            let one = function(&c.context, "one", "outlet:2");
            until(|| one.snapshot().power().is_some()).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let current = c.context.directory.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            c.cloud.lock().unwrap().read_status = 503;
            current.refresh_property(
                "outlet:2",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            );
            c.clock.advance(Duration::from_secs(1));
            until(|| {
                published.borrow().iter().any(|status| {
                    status
                        .pending
                        .iter()
                        .any(|pending| pending.did == "one" && pending.reason.contains("503"))
                })
            })
            .await;
            c.cloud.lock().unwrap().read_status = 200;
            current.refresh_property(
                "outlet:2",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            );
            c.clock.advance(Duration::from_secs(3));
            until(|| {
                published
                    .borrow()
                    .last()
                    .is_some_and(|status| status.pending.is_empty())
            })
            .await;
        },
    ));
}

#[test]
fn real_command_receipts_and_callback_events_are_published_without_queries() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let context = rig.source.context.clone();
    let published = Rc::new(RefCell::new(Vec::<AdapterStatus>::new()));
    let reports = published.clone();
    block_on(rig.run_with_status(
        move |status| {
            let completed = status
                .devices
                .iter()
                .any(|device| device.commands.iter().any(|command| command.last.is_some()));
            reports.borrow_mut().push(status);
            if completed && context.directory.borrow().problem().is_none() {
                context.set_catalog_problem(Some("callback observed receipt".into()));
            }
        },
        |c| async move {
            let one = function(&c.context, "one", "outlet:2");
            until(|| one.snapshot().availability == Availability::Available).await;
            one.execute(Command::On).await.unwrap();
            until(|| {
                published.borrow().iter().any(|status| {
                    status.devices.iter().any(|device| {
                        device.commands.iter().any(|command| {
                            command.last.as_ref().is_some_and(|last| {
                                last.actual_path == Some(PathKind::Cloud)
                                    && last.result == Some(Ok(()))
                            })
                        })
                    })
                })
            })
            .await;
            until(|| {
                published.borrow().iter().any(|status| {
                    status.catalog_problem.as_deref() == Some("callback observed receipt")
                })
            })
            .await;
            let count = published.borrow().len();
            c.context.status_changed.notify(usize::MAX);
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(published.borrow().len(), count);
        },
    ));
}
