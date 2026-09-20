use super::*;

#[test]
fn runtime_run_owns_the_lan_session_through_admission_state_and_control() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let identity = crate::xiaomi::certificate::ClientIdentity::generate("10001").unwrap();
    let certificate = crate::xiaomi::test_support::sign_csr(
        &identity.csr_pem,
        unix_time() - 60,
        unix_time() + 30 * 24 * 60 * 60,
    )
    .unwrap();
    store
        .xiaomi()
        .replace(&crate::storage::XiaomiRecord {
            uid: "10001".into(),
            region: "cn".into(),
            oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
            redirect_uri:
                "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                    .into(),
            tokens: crate::storage::TokenSet {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: unix_time() + 30 * 24 * 60 * 60,
                refresh_at: unix_time() + 20 * 24 * 60 * 60,
            },
            virtual_did: identity.virtual_did,
            private_key_pem: identity.private_key_pem,
            certificate_pem: certificate,
        })
        .unwrap();
    let account = crate::device::AccountId::new("10001").unwrap();
    let home = crate::device::HomeId::new("home").unwrap();
    let auth = store.xiaomi().snapshot().unwrap();
    assert!(
        store
            .devices()
            .publish_topology(&crate::storage::PublishedTopologyDelta {
                account: account.clone(),
                session_generation: auth.session_generation,
                binding: Some(crate::storage::HomeBinding {
                    account: account.clone(),
                    home: home.clone(),
                    display_name: "Home".into(),
                }),
                devices: vec![],
                definitions: vec![],
                deactivate: vec![],
            })
            .unwrap()
            .is_some()
    );
    let runtime = XiaomiRuntime::new(store.clone()).unwrap();
    let (session, handle, notifications, lan_device, token) =
        crate::xiaomi::lan::session_pair("lumi.acpartner.mcn02");
    let address = match lan_device.get_ref().local_addr().unwrap() {
        std::net::SocketAddr::V4(address) => address,
        std::net::SocketAddr::V6(_) => unreachable!(),
    };
    let interface = crate::xiaomi::discovery::NetworkInterface {
        index: 1,
        name: "en-test".into(),
        address: "192.0.2.2".parse().unwrap(),
        netmask: "0.0.0.0".parse().unwrap(),
        prefix_len: 0,
    };
    let target = crate::xiaomi::lan::LanTarget::for_test(
        42,
        "lumi.acpartner.mcn02",
        address,
        interface.clone(),
        NetworkEpoch::new(7),
        token,
    );
    let type_urn = "urn:miot-spec-v2:device:air-conditioner:0000A004:lumi-mcn02:1";
    let spec = include_str!("../../../../../tests/fixtures/miot_specs/lumi.acpartner.mcn02.json");
    let owned = crate::xiaomi::cloud::OwnedCatalog {
        uid: "10001".into(),
        homes: vec![crate::xiaomi::cloud::OwnedHome {
            id: "home".into(),
            name: "Home".into(),
            group_id: "group".into(),
            dids: vec!["42".into()],
            rooms: vec![],
        }],
        devices: vec![crate::xiaomi::cloud::CloudDevice {
            did: "42".into(),
            uid: Some("10001".into()),
            name: "Air conditioner".into(),
            model: "lumi.acpartner.mcn02".into(),
            spec_type: Some(type_urn.into()),
            pid: Some(0),
            token: Some(crate::storage::DeviceToken(token.to_vec())),
            online: Some(true),
            local_ip: Some(address.ip().to_string()),
            parent_id: None,
        }],
    };
    let specifications = HashMap::from([(type_urn.to_owned(), spec.to_owned())]);
    let catalog = crate::xiaomi::runtime::AdmissionCatalog {
        account: account.clone(),
        session_generation: auth.session_generation,
        catalog: assemble_catalog(&owned, &specifications).unwrap(),
        specifications,
    };
    let descriptor = catalog.catalog.devices[0]
        .features
        .iter()
        .find(|feature| feature.definition.role == crate::device::FeatureRole::Climate)
        .unwrap()
        .clone();
    let property = descriptor
        .binding
        .properties
        .iter()
        .find(|property| property.readable)
        .map(|property| LanProperty {
            siid: property.siid,
            piid: property.piid,
        })
        .unwrap();
    let network = NetworkUpdate {
        epoch: NetworkEpoch::new(7),
        snapshot: crate::xiaomi::discovery::NetworkSnapshot::select(
            vec![crate::xiaomi::discovery::InterfaceRecord {
                index: 1,
                name: "en-test".into(),
                address: "192.0.2.2".parse().unwrap(),
                netmask: "0.0.0.0".parse().unwrap(),
                up: true,
                point_to_point: false,
                loopback: false,
                link_type: crate::xiaomi::discovery::LinkType::Ethernet,
                physical: true,
            }],
            None,
        )
        .unwrap(),
    };
    let startup = async move {
        crate::xiaomi::runtime::start_lan_session_for_test(
            session,
            handle,
            notifications,
            property,
            Instant::now() + Duration::from_secs(1),
            crate::xiaomi::lan::LanSendGuard::new(),
        )
        .await
    }
    .boxed_local();
    let lifecycle_control;
    {
        let mut holder = runtime.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        runner.sessions.admission.invalidate(NetworkEpoch::new(7));
        runner.sessions.catalog = Some(catalog.clone());
        runner.discovery.set_network(network.clone());
        runner
            .sessions
            .admission
            .observe_cloud(&CloudEvidence {
                account: account.clone(),
                session_generation: auth.session_generation,
                status: CloudStatus::Ready,
            })
            .unwrap();
        runner
            .discovery
            .defer_network_until(Instant::now() + Duration::from_secs(60));
        runner
            .auth
            .defer_until(Instant::now() + Duration::from_secs(60));
        runner
            .catalog_refresh
            .defer_until(Instant::now() + Duration::from_secs(60));
        runner
            .discovery
            .defer_browser_until(Instant::now() + Duration::from_secs(60));
        runtime.inner.refresh_requested.set(false);
        let control = LanConnectionControl::new(runtime.inner.wake.clone());
        lifecycle_control = control.clone();
        let (fact_sender, fact_receiver) = flume::bounded(2);
        let physical = crate::device::PhysicalDeviceId {
            account: account.clone(),
            home,
            parent_did: crate::device::DeviceDid::new("42").unwrap(),
        };
        let config = LanConnectionConfig {
            physical: physical.clone(),
            targets: vec![target.clone()],
            network,
            account,
            session_generation: auth.session_generation,
            descriptor,
            property,
            virtual_did: auth.record.as_ref().unwrap().virtual_did.parse().unwrap(),
            setup: runner.sessions.lan_setup.clone(),
            startup: RefCell::new(Some(startup)),
        };
        runner.sessions.lans.insert(
            physical,
            ActiveLan {
                targets: vec![target],
                control: control.clone(),
                current: None,
                task: lan_connection_lifetime(
                    config,
                    control,
                    fact_sender,
                    runtime.inner.registry.clone(),
                    runtime.inner.state.clone(),
                ),
                fact: lan_connection_fact(fact_receiver),
            },
        );
    }
    let application_done = Rc::new(Cell::new(false));
    let application_completed = application_done.clone();
    let wire_stage = Rc::new(Cell::new(0_u8));
    let simulated_stage = wire_stage.clone();
    let subscribe_count = Rc::new(Cell::new(0_u8));
    let simulated_subscribe_count = subscribe_count.clone();
    let unsubscribe_count = Rc::new(Cell::new(0_u8));
    let simulated_unsubscribe_count = unsubscribe_count.clone();
    let application = async {
        let feature = loop {
            if let Some(feature) = runtime
                .service()
                .features()
                .into_iter()
                .find(|feature| feature.identity.role == crate::device::FeatureRole::Climate)
            {
                break feature.identity;
            }
            Timer::after(Duration::from_millis(10)).await;
        };
        assert!(
            RuntimeTransports::new(runtime.inner.registry.clone())
                .available_paths(&feature.physical)
                .cloud,
            "LAN admission did not publish the authorized Cloud fallback"
        );
        loop {
            let current = runtime
                .service()
                .snapshot(&feature)
                .and_then(|snapshot| snapshot.property(crate::device::Property::Power).cloned())
                .is_some_and(|state| {
                    matches!(
                        state,
                        crate::device::PropertyState::Current {
                            value: crate::device::PropertyValue::Power(true),
                            ..
                        }
                    )
                });
            if current {
                break;
            }
            Timer::after(Duration::from_millis(10)).await;
        }
        while !lifecycle_control.push_active() {
            Timer::after(Duration::from_millis(5)).await;
        }
        let accepted = runtime
            .service()
            .command(&feature, crate::device::DeviceCommand::SetPower(false))
            .await;
        assert_eq!(accepted, crate::device::CommandOutcome::Accepted);
        lifecycle_control.set_desired_push(false);
        while lifecycle_control.push_active() {
            Timer::after(Duration::from_millis(5)).await;
        }
        application_completed.set(true);
        runtime.stop();
    };
    let simulated_device = async {
        let mut accepted_control = false;
        let mut unsubscribed = false;
        while !accepted_control || !unsubscribed {
            let (request, source) =
                crate::xiaomi::lan::receive_request_for_test(&lan_device, &token).await;
            simulated_stage.set(simulated_stage.get().saturating_add(1));
            let method = request["method"].as_str().unwrap();
            if method == "miIO.sub" {
                simulated_subscribe_count.set(simulated_subscribe_count.get().saturating_add(1));
                Timer::after(Duration::from_millis(20)).await;
            } else if method == "miIO.unsub" {
                simulated_unsubscribe_count
                    .set(simulated_unsubscribe_count.get().saturating_add(1));
                unsubscribed = true;
            }
            let response = match method {
                "get_properties" => serde_json::json!({
                    "id":request["id"],"error":{"code":-1}
                }),
                "get_prop" => serde_json::json!({
                    "id":request["id"],
                    "result":["on","cool",25,"small_fan","off"]
                }),
                "miIO.sub" => serde_json::json!({
                    "id":request["id"],"result":{"code":0}
                }),
                "miIO.unsub" => serde_json::json!({
                    "id":request["id"],"result":{"code":0}
                }),
                "set_power" => {
                    assert_eq!(request["params"], serde_json::json!(["off"]));
                    accepted_control = true;
                    serde_json::json!({"id":request["id"],"result":["ok"]})
                }
                method => panic!("unexpected LAN method {method}"),
            };
            crate::xiaomi::lan::reply_for_test(&lan_device, &token, source, 100, response).await;
        }
    };
    block_on(future::race(
        async {
            future::or(
                async {
                    let result = runtime.run().await;
                    if !application_done.get() {
                        panic!("XiaomiRuntime returned before the LAN application: {result:?}");
                    }
                    result.unwrap();
                },
                async {
                    future::zip(application, simulated_device).await;
                },
            )
            .await;
        },
        async {
            Timer::after(Duration::from_secs(5)).await;
            panic!(
                "XiaomiRuntime LAN lifecycle exceeded its hard deadline: wire_stage={}, features={}, admission={:?}, publication={}, ready_seen={}, ready_stage={}",
                wire_stage.get(),
                runtime.service().features().len(),
                runtime.status().admission.status,
                // Publication remains pending while storage is Busy and becomes rejected
                // only when the authenticated proof is no longer eligible.
                lifecycle_control.publication() as u8,
                lifecycle_control.ready_seen.get(),
                lifecycle_control.ready_stage.get(),
            )
        },
    ));
    assert!(application_done.get());
    assert_eq!(
        subscribe_count.get(),
        1,
        "notification cancelled an in-flight SUB"
    );
    assert_eq!(
        unsubscribe_count.get(),
        1,
        "source deselection must send one matching UNSUB"
    );
}
