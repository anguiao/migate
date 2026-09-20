use super::*;

#[test]
fn manual_refresh_schedules_one_forced_state_read_per_admitted_device() {
    let mut response_index = 0;
    let (cloud_base, requests) =
        crate::xiaomi::test_support::dynamic_mock_server(2, move |request| {
            assert!(request.target.ends_with("/get"));
            let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
            let value = response_index != 0;
            response_index += 1;
            let result = body["params"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "did": item["did"],
                        "siid": item["siid"],
                        "piid": item["piid"],
                        "code": 0,
                        "value": value,
                    })
                })
                .collect::<Vec<_>>();
            crate::xiaomi::test_support::MockResponse::json(
                200,
                &serde_json::json!({"code": 0, "result": result}).to_string(),
            )
        });
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let runtime = XiaomiRuntime::new(store.clone()).unwrap();
    let service = runtime.service();
    let physical = crate::device::PhysicalDeviceId {
        account: crate::device::AccountId::new("account").unwrap(),
        home: crate::device::HomeId::new("home").unwrap(),
        parent_did: crate::device::DeviceDid::new("device").unwrap(),
    };
    let descriptors = crate::xiaomi::catalog::compile_spec(
        "xiaomi.switch.w3",
        include_str!("../../../../../tests/fixtures/miot_specs/xiaomi.switch.w3.json"),
    )
    .unwrap()
    .features;
    assert!(descriptors.len() > 1);
    let auth_generation = store.xiaomi().snapshot().unwrap().session_generation;
    let features = descriptors
        .into_iter()
        .take(2)
        .map(|descriptor| {
            let identity = crate::device::FeatureIdentity {
                physical: physical.clone(),
                service_instance: descriptor.definition.service_instance,
                role: descriptor.definition.role,
            };
            service.publish(
                identity.clone(),
                descriptor.definition.name.clone(),
                descriptor.definition.capabilities.clone(),
            );
            let registered = RuntimeFeature {
                identity: identity.clone(),
                descriptor,
                authority_generation: 1,
                auth_session_generation: auth_generation,
            };
            AdmissionFeature {
                identity,
                runtime: registered,
                paths: OperationPaths {
                    cloud: true,
                    ..OperationPaths::default()
                },
                gateways: Vec::new(),
                lan_evidence: None,
            }
        })
        .collect::<Vec<_>>();
    let snapshot = AdmissionSnapshot {
        binding: None,
        status: AdmissionStatus::Active,
        epoch: NetworkEpoch::new(1),
        features,
    };
    runtime.inner.registry.install_cloud(
        physical.clone(),
        Rc::new(CloudClient::for_test(&cloud_base, Duration::from_millis(300)).unwrap()),
        "access",
        unix_time() + 60,
        SessionAuthority::new(),
    );
    runtime.inner.state.reconcile(&snapshot);
    block_on(runtime.inner.state.run_until_idle());
    let initial_requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(initial_requests.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&initial_requests[0].body).unwrap()["params"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let token = runtime
        .inner
        .state
        .select_push_source(&physical, PushSource::Cloud, 1, 1)
        .unwrap();
    assert!(runtime.inner.state.acknowledge(&token, 1, 1));
    block_on(runtime.inner.state.run_until_idle());
    assert!(requests.try_recv().is_err());
    runtime.inner.status.borrow_mut().admission = snapshot.clone();

    runtime.refresh();
    runtime.refresh();
    block_on(runtime.inner.state.run_until_idle());

    let refresh_requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(refresh_requests.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&refresh_requests[0].body).unwrap()["params"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(runtime.inner.refresh_requested.get());
    for feature in &snapshot.features {
        assert!(matches!(
            service
                .snapshot(&feature.identity)
                .unwrap()
                .property(crate::device::Property::Power),
            Some(crate::device::PropertyState::Current {
                value: crate::device::PropertyValue::Power(true),
                ..
            })
        ));
    }
}

#[test]
fn signed_out_runtime_remains_alive_until_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let runtime = XiaomiRuntime::new(store).unwrap();
    block_on(async {
        let run = runtime.run();
        futures_lite::pin!(run);
        assert!(poll_once(run.as_mut()).await.is_none());
        Timer::after(Duration::from_millis(20)).await;
        assert!(poll_once(run.as_mut()).await.is_none());
        runtime.stop();
        future::or(async { run.await.unwrap() }, async {
            Timer::after(Duration::from_millis(500)).await;
            panic!("stopped Xiaomi runtime did not exit")
        })
        .await;
    });
}

#[test]
fn connection_retries_back_off_to_a_bounded_maximum_and_reset_after_success() {
    let mut retry = Duration::from_secs(5);
    assert_eq!(
        advance_retry(&mut retry, Duration::from_secs(5)),
        Duration::from_secs(5)
    );
    assert_eq!(
        advance_retry(&mut retry, Duration::from_secs(5)),
        Duration::from_secs(10)
    );
    retry = Duration::from_secs(5);
    assert_eq!(
        advance_retry(&mut retry, Duration::from_secs(5)),
        Duration::from_secs(5)
    );
    retry = Duration::from_secs(5 * 60);
    assert_eq!(
        advance_retry(&mut retry, Duration::from_secs(5)),
        Duration::from_secs(5 * 60)
    );
    assert_eq!(retry, Duration::from_secs(5 * 60));
}

#[test]
fn safe_diagnostics_preserve_full_width_protocol_business_codes() {
    for code in [i64::MIN, i64::MAX] {
        let gateway = crate::xiaomi::gateway::GatewayErrorKind::Business(code);
        let lan = crate::xiaomi::lan::LanErrorKind::Business(code);
        let startup = TransportStartupError::Gateway(
            crate::xiaomi::gateway::GatewayError::for_test(gateway.clone()),
        );
        for diagnostic in [
            gateway_failure_code(&gateway),
            lan_failure_code(&lan),
            startup_failure_code(&startup),
        ] {
            let XiaomiSafeFailureCode::Rejected(actual) = diagnostic else {
                panic!("expected a protocol business rejection")
            };
            assert_eq!(i128::from(actual), i128::from(code));
        }
    }
}

#[test]
fn dropping_run_revokes_the_runtime_scope() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let runtime = XiaomiRuntime::new(store).unwrap();
    block_on(async {
        let mut run = Box::pin(runtime.run());
        assert!(poll_once(run.as_mut()).await.is_none());
        drop(run);
    });
    assert!(!runtime.inner.running.get());
    assert!(runtime.inner.stopped.get());
}

#[test]
fn authenticated_catalog_without_a_hub_does_not_starve_stop() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let runtime = XiaomiRuntime::new(store).unwrap();
    let snapshot = crate::xiaomi::discovery::NetworkSnapshot::select(
        vec![crate::xiaomi::discovery::InterfaceRecord {
            index: 7,
            name: "en-test".into(),
            address: "192.0.2.2".parse().unwrap(),
            netmask: "255.255.255.0".parse().unwrap(),
            up: true,
            point_to_point: false,
            loopback: false,
            link_type: crate::xiaomi::discovery::LinkType::Ethernet,
            physical: true,
        }],
        None,
    )
    .unwrap();
    {
        let mut holder = runtime.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        let auth = runner.auth.current_snapshot().unwrap();
        runner.sessions.catalog = Some(crate::xiaomi::runtime::AdmissionCatalog {
            account: crate::device::AccountId::new("signed-out-placeholder").unwrap(),
            session_generation: auth.session_generation,
            catalog: crate::xiaomi::catalog::DeviceCatalog {
                uid: "signed-out-placeholder".into(),
                homes: vec![],
                devices: vec![],
            },
            specifications: HashMap::new(),
        });
        runner.discovery.set_network(NetworkUpdate {
            epoch: NetworkEpoch::new(1),
            snapshot: snapshot.clone(),
        });
        runner
            .discovery
            .set_registry(DiscoveryRegistry::new(NetworkEpoch::new(1), snapshot));
        runner
            .discovery
            .defer_network_until(Instant::now() + Duration::from_secs(60));
        runner
            .auth
            .defer_until(Instant::now() + Duration::from_secs(60));
        runner
            .catalog_refresh
            .defer_until(Instant::now() + Duration::from_secs(60));
        runtime.inner.refresh_requested.set(false);
    }
    block_on(async {
        let completed = future::or(
            async {
                runtime.run().await.unwrap();
                true
            },
            async {
                Timer::after(Duration::from_millis(30)).await;
                runtime.stop();
                Timer::after(Duration::from_millis(470)).await;
                false
            },
        )
        .await;
        assert!(completed, "empty gateway retry work starved runtime stop");
    });
}
