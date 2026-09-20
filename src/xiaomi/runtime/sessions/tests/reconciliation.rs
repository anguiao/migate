use super::*;

#[test]
fn authoritative_owned_catalog_revokes_routes_before_spec_resolution() {
    exercise_catalog_and_subscription_completion(false, false, false, false);
}

#[test]
fn rejected_gateway_push_subscription_keeps_gateway_control_alive() {
    exercise_catalog_and_subscription_completion(true, false, false, false);
}

#[test]
fn session_policy_retracts_a_timed_out_lan_route_before_cloud_fallback() {
    exercise_catalog_and_subscription_completion(false, true, false, false);
}

#[test]
fn refreshed_gateway_admission_publishes_the_cloud_fallback_before_state() {
    exercise_catalog_and_subscription_completion(false, false, true, false);
}

#[test]
fn cloud_session_restores_online_state_on_the_same_mqtt_session() {
    exercise_catalog_and_subscription_completion(false, false, false, true);
}

fn exercise_catalog_and_subscription_completion(
    reject_subscription: bool,
    lan_timeout: bool,
    refresh_cloud: bool,
    cloud_lifecycle: bool,
) {
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
    let runtime = XiaomiRuntime::new(store.clone()).unwrap();
    let snapshot = store.xiaomi().snapshot().unwrap();
    let type_urn = "urn:miot-spec-v2:device:light:0000A001:yeelink-ml9:1";
    let initial_owned = crate::xiaomi::cloud::OwnedCatalog {
        uid: "10001".into(),
        homes: vec![crate::xiaomi::cloud::OwnedHome {
            id: "home".into(),
            name: "Home".into(),
            group_id: "group".into(),
            dids: vec!["device.did".into()],
            rooms: vec![],
        }],
        devices: vec![crate::xiaomi::cloud::CloudDevice {
            did: "device.did".into(),
            uid: Some("10001".into()),
            name: "Light".into(),
            model: "yeelink.light.ml9".into(),
            spec_type: Some(type_urn.into()),
            pid: None,
            token: None,
            online: Some(true),
            local_ip: None,
            parent_id: None,
        }],
    };
    let specifications = HashMap::from([(
        type_urn.to_owned(),
        include_str!("../../../../../tests/fixtures/miot_specs/yeelink.light.ml9.json").to_owned(),
    )]);
    let catalog = crate::xiaomi::runtime::AdmissionCatalog {
        account: crate::device::AccountId::new("10001").unwrap(),
        session_generation: snapshot.session_generation,
        catalog: assemble_catalog(&initial_owned, &specifications).unwrap(),
        specifications,
    };
    let network = crate::xiaomi::discovery::NetworkSnapshot::select(
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
    let endpoint = GatewayEndpoint {
        interface_index: 7,
        source_address: "192.0.2.2".parse().unwrap(),
        address: "192.0.2.10".parse().unwrap(),
        port: 8883,
    };
    let proof = AuthenticatedGateway {
        account: catalog.account.clone(),
        session_generation: snapshot.session_generation,
        candidate: GatewayCandidate {
            gateway_did: 123,
            home_group: "group".into(),
            endpoints: vec![endpoint.clone()],
            unverified: false,
        },
        selected_endpoint: endpoint,
        network,
        evidence: crate::xiaomi::gateway::GatewayEvidence {
            gateway_did: 123,
            peer_did: "123".into(),
            epoch: NetworkEpoch::new(7),
            devices: vec![crate::xiaomi::gateway::GatewayDevice {
                did: "device.did".into(),
                name: "Light".into(),
                urn: type_urn.into(),
                model: "yeelink.light.ml9".into(),
                online: Some(true),
                spec_v2_access: Some(true),
                push_available: Some(true),
            }],
        },
    };
    let (_mqtt, mqtt, messages) = crate::xiaomi::mqtt::MqttConnection::new(
        MqttConfig::new("catalog-test", None, Duration::from_secs(60))
            .with_endpoint("127.0.0.1", 9),
    )
    .unwrap();
    let (_session, handle, notifications) = crate::xiaomi::gateway::GatewaySession::new(
        "catalog-test",
        123,
        "123",
        NetworkEpoch::new(7),
        mqtt,
        messages,
    )
    .unwrap();
    let authority = SessionAuthority::new();
    let mut holder = runtime.inner.runner.borrow_mut();
    let runner = holder.as_mut().unwrap();
    runner.sessions.admission.invalidate(NetworkEpoch::new(7));
    runner.discovery.set_network(NetworkUpdate {
        epoch: NetworkEpoch::new(7),
        snapshot: proof.network.clone(),
    });
    runner
        .sessions
        .admission
        .observe_gateway(proof.clone(), &catalog)
        .unwrap();
    let admitted = runner.sessions.admission.snapshot().unwrap();
    runner.sessions.catalog = Some(catalog);
    let parts = crate::xiaomi::runtime::GatewayRuntimeParts {
        handle,
        notifications,
        evidence: proof.evidence.clone(),
        task: future::pending().boxed_local(),
    };
    let mut gateway = active_gateway(proof, authority, parts);
    gateway.wire_generation = 1;
    gateway.selected_dids = BTreeSet::from(["device.did".into()]);
    runner.sessions.gateways.insert(123, gateway);
    runner.sessions.rebuild_gateway_routes(
        &runtime.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
        &admitted,
    );
    assert_eq!(runner.sessions.gateways[&123].desired_dids.len(), 1);
    runtime.inner.state.reconcile(&admitted);
    let device = admitted.features[0].identity.physical.clone();
    let token = runtime
        .inner
        .state
        .select_push_source(&device, PushSource::Gateway(123), 123, 1)
        .unwrap();
    assert!(runtime.inner.state.acknowledge(&token, 123, 1));
    runner
        .sessions
        .gateways
        .get_mut(&123)
        .unwrap()
        .tokens
        .insert(device.clone(), token);
    if refresh_cloud {
        runner
            .sessions
            .admission
            .observe_cloud(&CloudEvidence {
                account: crate::device::AccountId::new("10001").unwrap(),
                session_generation: snapshot.session_generation,
                status: CloudStatus::Ready,
            })
            .unwrap();
        runtime.inner.registry.revoke_cloud(&device);
        runner.sessions.cloud_routes.clear();
        let evidence = runner.sessions.gateways[&123].proof.evidence.clone();
        runner
            .sessions
            .finish_gateway_operation(
                &runtime.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
                GatewayOperationResult::Refreshed {
                    did: 123,
                    result: Ok(evidence),
                },
            )
            .unwrap();
        assert!(
            RuntimeTransports::new(runtime.inner.registry.clone())
                .available_paths(&device)
                .cloud
        );
        return;
    }
    if lan_timeout {
        runner
            .sessions
            .admission
            .observe_cloud(&CloudEvidence {
                account: crate::device::AccountId::new("10001").unwrap(),
                session_generation: snapshot.session_generation,
                status: CloudStatus::Ready,
            })
            .unwrap();
        runner.sessions.admission.remove_gateway(123).unwrap();
        let cloud_snapshot = runner.sessions.admission.snapshot().unwrap();
        runtime.inner.state.reconcile(&cloud_snapshot);
        runner.sessions.rebuild_gateway_routes(
            &runtime.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
            &cloud_snapshot,
        );
        let lan_feature = cloud_snapshot.features[0].runtime.clone();
        runtime.inner.commands.register(lan_feature);
        runtime
            .service()
            .set_state_availability(&cloud_snapshot.features[0].identity, true);

        let (session, handle, _notifications, lan_device, token) =
            crate::xiaomi::lan::session_pair("lumi.acpartner.mcn02");
        let mut lan_task = session.run().boxed_local();
        let evidence = block_on(future::race(
            async {
                future::zip(
                    handle.authenticate(
                        crate::xiaomi::lan::LanProperty { siid: 2, piid: 1 },
                        Instant::now() + Duration::from_secs(1),
                        crate::xiaomi::lan::LanSendGuard::new(),
                    ),
                    crate::xiaomi::lan::reject_native_authentication_probe(&lan_device, &token),
                )
                .await
                .0
                .unwrap()
            },
            async {
                let result = lan_task.as_mut().await;
                panic!("LAN session stopped during authentication: {result:?}")
            },
        ));
        assert!(!evidence.native_supported);
        let lan_authority = SessionAuthority::new();
        let target = crate::xiaomi::lan::LanTarget::for_test(
            42,
            "lumi.acpartner.mcn02",
            "127.0.0.1:54321".parse().unwrap(),
            crate::xiaomi::discovery::NetworkInterface {
                index: 1,
                name: "test-loopback".into(),
                address: std::net::Ipv4Addr::LOCALHOST,
                netmask: "255.0.0.0".parse().unwrap(),
                prefix_len: 8,
            },
            NetworkEpoch::new(7),
            [0x31; 16],
        );
        let control = LanConnectionControl::new(runtime.inner.wake.clone());
        control.activate();
        let resources = Rc::new(LanAttemptResources::new(
            device.clone(),
            handle,
            lan_authority,
            runtime.inner.registry.clone(),
            runtime.inner.state.clone(),
        ));
        resources.install_route(Some(cloud_snapshot.features[0].runtime.descriptor.clone()));
        let (_fact_sender, fact_receiver) = flume::bounded(1);
        runner.sessions.lans.insert(
            device.clone(),
            ActiveLan {
                targets: vec![target],
                control,
                current: Some((1, resources)),
                task: async move {
                    let _ = lan_task.await;
                }
                .boxed_local(),
                fact: lan_connection_fact(fact_receiver),
            },
        );
        let (cloud_base, cloud_requests) =
            crate::xiaomi::test_support::mock_server_with_accept_timeout(
                vec![crate::xiaomi::test_support::MockResponse::json(
                    200,
                    r#"{"code":0,"result":[{"did":"device.did","siid":2,"piid":2,"code":0}]}"#,
                )],
                Duration::from_secs(5),
            );
        let cloud_authority = SessionAuthority::new();
        runtime.inner.registry.install_cloud(
            device.clone(),
            Rc::new(CloudClient::for_test(&cloud_base, Duration::from_millis(500)).unwrap()),
            "access",
            i64::MAX,
            cloud_authority.clone(),
        );
        let failed = runtime.service().command(
            &cloud_snapshot.features[0].identity,
            crate::device::DeviceCommand::SetPower(true),
        );
        let fallback = runtime.service().command(
            &cloud_snapshot.features[0].identity,
            crate::device::DeviceCommand::SetBrightness(crate::device::Percent::new(25.0).unwrap()),
        );
        let completed = block_on(future::race(
            async {
                let task = runner.sessions.lans.get_mut(&device).unwrap().task.as_mut();
                let application = async {
                    future::zip(runtime.inner.commands.run_until_idle(), async {
                        let (request, _) =
                            crate::xiaomi::lan::receive_request_for_test(&lan_device, &token).await;
                        assert_eq!(request["method"], "set_power");
                        assert_eq!(request["params"], serde_json::json!(["on"]));
                    })
                    .await;
                    true
                };
                future::or(application, async {
                    task.await;
                    panic!("LAN session stopped before the timed-out command")
                })
                .await
            },
            async {
                Timer::after(Duration::from_secs(5)).await;
                false
            },
        ));
        assert!(
            completed,
            "LAN timeout command did not finish within its hard deadline"
        );
        assert_eq!(block_on(failed), crate::device::CommandOutcome::Ambiguous);
        let fallback_outcome = block_on(fallback);
        let request = cloud_requests.recv_timeout(Duration::from_secs(1));
        let request_received = request.is_ok();
        assert_eq!(
            fallback_outcome,
            crate::device::CommandOutcome::Accepted,
            "queued fallback request_received={request_received}, authority={:?}, can_control={}, diagnostics={:?}",
            cloud_authority.check(),
            runtime
                .service()
                .can_control(&cloud_snapshot.features[0].identity),
            runtime.inner.commands.drain_diagnostics(),
        );
        let request = request.expect("queued cloud fallback did not send a loopback request");
        assert!(request.target.ends_with("/miotspec/prop/set"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&request.body).unwrap()["params"][0]["value"],
            serde_json::json!(25)
        );
        assert!(
            cloud_requests.try_recv().is_err(),
            "the queued target was sent to Cloud more than once"
        );
        runner
            .sessions
            .drain_transport_failures(&runtime.session_context(
                &runner.auth,
                &runner.discovery,
                &runner.catalog_refresh,
            ))
            .unwrap();
        assert!(runner.sessions.lans[&device].current.is_none());
        assert!(runner.sessions.lans[&device].control.generation() > 1);
        return;
    }
    runner
        .sessions
        .finish_gateway_operation(
            &runtime.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
            GatewayOperationResult::Selected {
                did: 123,
                desired: BTreeSet::from(["device.did".into()]),
                result: if reject_subscription {
                    Err(crate::xiaomi::gateway::GatewayError::for_test(
                        crate::xiaomi::gateway::GatewayErrorKind::Transport,
                    ))
                } else {
                    Ok(2)
                },
            },
        )
        .unwrap();
    if reject_subscription {
        assert!(runner.sessions.gateways.contains_key(&123));
        assert_eq!(runner.sessions.gateway_routes.get(&device), Some(&123));
        assert!(runner.sessions.gateways[&123].tokens.is_empty());
        assert!(
            runtime
                .status()
                .diagnostics
                .iter()
                .any(|diagnostic| matches!(
                    diagnostic,
                    XiaomiRuntimeDiagnostic::Boundary(XiaomiBoundaryDiagnostic {
                        component: XiaomiRuntimeComponent::Gateway,
                        stage: XiaomiFailureStage::Subscribe,
                        code: XiaomiSafeFailureCode::Unavailable,
                        subject: Some(subject),
                    }) if subject == "123"
                ))
        );
        return;
    }
    runner
        .sessions
        .apply_gateway_notification(
            &runtime.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
            123,
            GatewayNotification::Property {
                did: "device.did".into(),
                siid: 2,
                piid: 1,
                value: Some(crate::xiaomi::catalog::WireValue::Boolean(true)),
                epoch: 7,
                generation: 2,
            },
        )
        .unwrap();
    let feature = admitted.features[0].identity.clone();
    assert!(
        runtime
            .service()
            .snapshot(&feature)
            .and_then(|snapshot| snapshot.property(crate::device::Property::Power).cloned())
            .is_some_and(|state| matches!(
                state,
                crate::device::PropertyState::Current {
                    value: crate::device::PropertyValue::Power(true),
                    ..
                }
            )),
        "a retained DID rejected the new subscription generation"
    );

    if cloud_lifecycle {
        runner
            .sessions
            .drop_gateway(
                &runtime.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
                123,
            )
            .unwrap();
        runner
            .sessions
            .admission
            .observe_cloud(&CloudEvidence {
                account: crate::device::AccountId::new("10001").unwrap(),
                session_generation: snapshot.session_generation,
                status: CloudStatus::Ready,
            })
            .unwrap();
        runner
            .sessions
            .reconcile(
                &runtime.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
                &runner.sessions.admission,
            )
            .unwrap();
        runner
            .sessions
            .sync_cloud_routes(&runtime.session_context(
                &runner.auth,
                &runner.discovery,
                &runner.catalog_refresh,
            ))
            .unwrap();
        let desired = BTreeSet::from(["device.did".to_owned()]);
        let authority = SessionAuthority::new();
        let CloudRuntimeHarness {
            handle,
            notifications,
            session,
            subscribed,
            commands: broker_commands,
            broker,
        } = real_cloud_runtime_connection("device.did");
        let control = CloudConnectionControl::new(desired.clone());
        let (facts, receiver) = flume::bounded(256);
        runner.sessions.cloud_notifications = Some(ActiveCloudNotifications {
            authority: authority.clone(),
            control: control.clone(),
            task: active_cloud_connection_lifetime(
                handle,
                authority,
                notifications,
                session,
                BTreeSet::new(),
                control,
                facts,
            ),
            fact: cloud_connection_fact(receiver),
            desired: BTreeSet::new(),
            generation: 0,
            session_id: 0,
            tokens: BTreeMap::new(),
            publication: None,
        });
        runner
            .auth
            .defer_until(Instant::now() + Duration::from_secs(60));
        runner
            .discovery
            .defer_network_until(Instant::now() + Duration::from_secs(60));
        runner
            .catalog_refresh
            .defer_until(Instant::now() + Duration::from_secs(60));
        runner
            .discovery
            .defer_browser_until(Instant::now() + Duration::from_secs(60));
        runner.auth.cancel();
        runner.discovery.cancel();
        runner.catalog_refresh.cancel();

        runtime.inner.refresh_requested.set(false);
        drop(holder);
        let completed = block_on(future::or(
            async {
                let run = async {
                    runtime.run().await.unwrap();
                };
                let application = async {
                    while subscribed.try_recv().is_err() {
                        Timer::after(Duration::from_millis(5)).await;
                    }
                    broker_commands
                        .send(CloudBrokerCommand::Acknowledge)
                        .unwrap();
                    wait_until(Duration::from_secs(1), || {
                        runtime
                            .inner
                            .state
                            .has_healthy_source_for_test(&feature.physical, PushSource::Cloud)
                    })
                    .await;
                    broker_commands
                        .send(CloudBrokerCommand::Property(false))
                        .unwrap();
                    wait_until(Duration::from_secs(1), || {
                        runtime
                            .service()
                            .snapshot(&feature)
                            .and_then(|snapshot| {
                                snapshot.property(crate::device::Property::Power).cloned()
                            })
                            .is_some_and(|state| {
                                matches!(
                                    state,
                                    crate::device::PropertyState::Current {
                                        value: crate::device::PropertyValue::Power(false),
                                        ..
                                    }
                                )
                            })
                    })
                    .await;
                    broker_commands.send(CloudBrokerCommand::Offline).unwrap();
                    wait_until(Duration::from_secs(1), || {
                        !runtime.service().is_available(&feature)
                    })
                    .await;
                    broker_commands.send(CloudBrokerCommand::Online).unwrap();
                    broker_commands
                        .send(CloudBrokerCommand::Property(true))
                        .unwrap();
                    wait_until(Duration::from_secs(1), || {
                        runtime.service().is_available(&feature)
                            && runtime
                                .service()
                                .snapshot(&feature)
                                .and_then(|snapshot| {
                                    snapshot.property(crate::device::Property::Power).cloned()
                                })
                                .is_some_and(|state| {
                                    matches!(
                                        state,
                                        crate::device::PropertyState::Current {
                                            value: crate::device::PropertyValue::Power(true),
                                            ..
                                        }
                                    )
                                })
                    })
                    .await;
                    runtime.stop();
                };
                future::zip(run, application).await;
                true
            },
            async {
                Timer::after(Duration::from_secs(3)).await;
                false
            },
        ));
        let _ = broker_commands.send(CloudBrokerCommand::Stop);
        broker.join().unwrap();
        assert!(
            completed,
            "Cloud session loopback exceeded its hard deadline"
        );
        return;
    }

    let replacement = crate::xiaomi::cloud::OwnedCatalog {
        uid: "10001".into(),
        homes: vec![crate::xiaomi::cloud::OwnedHome {
            id: "home".into(),
            name: "Home".into(),
            group_id: "group".into(),
            dids: vec!["unresolved.did".into()],
            rooms: vec![],
        }],
        devices: vec![crate::xiaomi::cloud::CloudDevice {
            did: "unresolved.did".into(),
            uid: Some("10001".into()),
            name: "Unresolved".into(),
            model: "vendor.unknown.x".into(),
            spec_type: Some("urn:miot-spec-v2:device:unknown:0000FFFF:vendor-x:1".into()),
            pid: None,
            token: None,
            online: Some(true),
            local_ip: None,
            parent_id: None,
        }],
    };
    runtime
        .finish_catalog_refresh(
            runner,
            CatalogTaskResult::Owned {
                snapshot,
                owned: Some(replacement),
            },
        )
        .unwrap();
    assert!(
        runner.catalog_refresh.running(),
        "spec resolution was not left pending"
    );
    if let Some(gateway) = runner.sessions.gateways.get(&123) {
        assert!(gateway.desired_dids.is_empty());
        assert!(gateway.control.desired_is_empty());
    }
    assert!(runtime.service().features().is_empty());
}
