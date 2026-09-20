use super::*;

#[test]
fn network_refresh_keeps_existing_cloud_routes_without_admitting_new_devices() {
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
    let auth = store.xiaomi().snapshot().unwrap();
    let document =
        include_str!("../../../../../tests/fixtures/miot_specs/yeelink.light.ml9.json").to_owned();
    let compiled = crate::xiaomi::catalog::compile_spec("yeelink.light.ml9", &document).unwrap();
    let account = crate::device::AccountId::new("10001").unwrap();
    let mut catalog = crate::xiaomi::runtime::AdmissionCatalog {
        account: account.clone(),
        session_generation: auth.session_generation,
        catalog: crate::xiaomi::catalog::DeviceCatalog {
            uid: "10001".into(),
            homes: vec![crate::xiaomi::catalog::CatalogHome {
                id: "home".into(),
                name: "Home".into(),
                group_id: "0011223344556677".into(),
                rooms: vec![],
            }],
            devices: vec![crate::xiaomi::catalog::CatalogDevice {
                home_id: "home".into(),
                room_id: None,
                parent_did: "device.did".into(),
                name: "Light".into(),
                model: "yeelink.light.ml9".into(),
                spec_type: Some(compiled.type_urn.clone()),
                pid: Some(0),
                token: None,
                online: Some(true),
                local_ip: None,
                parent_id: None,
                features: compiled.features.clone(),
            }],
        },
        specifications: HashMap::from([(compiled.type_urn.clone(), document)]),
    };
    let network = crate::xiaomi::discovery::NetworkSnapshot::select(
        vec![crate::xiaomi::discovery::InterfaceRecord {
            index: 1,
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
        interface_index: 1,
        source_address: "192.0.2.2".parse().unwrap(),
        address: "192.0.2.3".parse().unwrap(),
        port: 8883,
    };
    let proof = AuthenticatedGateway {
        account: account.clone(),
        session_generation: auth.session_generation,
        candidate: GatewayCandidate {
            gateway_did: 123,
            home_group: "0011223344556677".into(),
            endpoints: vec![endpoint.clone()],
            unverified: false,
        },
        selected_endpoint: endpoint,
        network: network.clone(),
        evidence: crate::xiaomi::gateway::GatewayEvidence {
            gateway_did: 123,
            peer_did: "123".into(),
            epoch: NetworkEpoch::new(7),
            devices: vec![crate::xiaomi::gateway::GatewayDevice {
                did: "device.did".into(),
                name: "Light".into(),
                urn: compiled.type_urn.clone(),
                model: "yeelink.light.ml9".into(),
                online: Some(true),
                spec_v2_access: Some(true),
                push_available: Some(false),
            }],
        },
    };

    let feature;
    {
        let mut holder = runtime.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        runner.sessions.admission.invalidate(NetworkEpoch::new(7));
        runner
            .sessions
            .admission
            .observe_gateway(proof, &catalog)
            .unwrap();
        runner
            .sessions
            .admission
            .observe_cloud(&CloudEvidence {
                account,
                session_generation: auth.session_generation,
                status: CloudStatus::Ready,
            })
            .unwrap();
        runner.sessions.admission.remove_gateway(123).unwrap();
        runner.sessions.catalog = Some(catalog.clone());
        runner.discovery.set_network(NetworkUpdate {
            epoch: NetworkEpoch::new(7),
            snapshot: network.clone(),
        });
        runner.discovery.set_epoch(7);
        runner
            .sessions
            .sync_cloud_routes(&runtime.session_context(
                &runner.auth,
                &runner.discovery,
                &runner.catalog_refresh,
            ))
            .unwrap();
        let admitted = runner.sessions.admission.snapshot().unwrap();
        let light = admitted
            .features
            .iter()
            .find(|candidate| candidate.identity.role == crate::device::FeatureRole::Light)
            .unwrap();
        feature = light.identity.clone();
        assert!(light.paths.cloud);
        assert!(
            RuntimeTransports::new(runtime.inner.registry.clone())
                .available_paths(&feature.physical)
                .cloud
        );
        runtime.inner.state.reconcile(&admitted);
        runtime
            .inner
            .service
            .apply_report(crate::device::StateReport {
                feature: feature.clone(),
                report_version: runtime.inner.service.next_report_version(),
                source: crate::device::StateSource::Cloud,
                observed_at: unix_time(),
                values: BTreeMap::from([(
                    crate::device::Property::Power,
                    crate::device::PropertyValue::Power(true),
                )]),
            });
        runtime.inner.state.reconcile(&admitted);
        assert!(runtime.inner.service.is_available(&feature));

        runtime
            .finish_network_refresh(
                runner,
                Ok(NetworkUpdate {
                    epoch: NetworkEpoch::new(8),
                    snapshot: network,
                }),
            )
            .unwrap();
        let refreshed = runner.sessions.admission.snapshot().unwrap();
        assert!(
            refreshed
                .features
                .iter()
                .find(|candidate| candidate.identity == feature)
                .unwrap()
                .paths
                .cloud
        );
        assert!(
            RuntimeTransports::new(runtime.inner.registry.clone())
                .available_paths(&feature.physical)
                .cloud
        );
        assert!(runtime.inner.service.is_available(&feature));

        let mut unproved = catalog.catalog.devices[0].clone();
        unproved.parent_did = "new-cloud-only".into();
        unproved.name = "New cloud-only light".into();
        catalog.catalog.devices.push(unproved);
        runner
            .sessions
            .admission
            .apply_complete_catalog(&catalog)
            .unwrap();
    }
    assert!(
        runtime
            .service()
            .features()
            .iter()
            .all(|candidate| candidate.identity.physical.parent_did.as_str() != "new-cloud-only")
    );

    drop(runtime);
    let (cloud_base, _) = crate::xiaomi::test_support::dynamic_mock_server(10, |request| {
        assert!(request.target.ends_with("/get"));
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        let result = body["params"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                let piid = item["piid"].as_u64().unwrap();
                serde_json::json!({
                    "did":"device.did",
                    "siid":2,
                    "piid":piid,
                    "code":0,
                    "value":match piid {
                        1 => serde_json::json!(true),
                        2 => serde_json::json!(50),
                        3 => serde_json::json!(4000),
                        _ => serde_json::json!(0),
                    }
                })
            })
            .collect::<Vec<_>>();
        crate::xiaomi::test_support::MockResponse::json(
            200,
            &serde_json::json!({"code":0,"result":result}).to_string(),
        )
    });
    let restored = XiaomiRuntime::new(store.clone()).unwrap();
    let owned = crate::xiaomi::cloud::OwnedCatalog {
        uid: "10001".into(),
        homes: vec![crate::xiaomi::cloud::OwnedHome {
            id: "home".into(),
            name: "Home".into(),
            group_id: "0011223344556677".into(),
            dids: vec!["device.did".into(), "new-cloud-only".into()],
            rooms: vec![],
        }],
        devices: vec![
            crate::xiaomi::cloud::CloudDevice {
                did: "device.did".into(),
                uid: Some("10001".into()),
                name: "Light".into(),
                model: "yeelink.light.ml9".into(),
                spec_type: Some(compiled.type_urn.clone()),
                pid: Some(0),
                token: None,
                online: Some(true),
                local_ip: None,
                parent_id: None,
            },
            crate::xiaomi::cloud::CloudDevice {
                did: "new-cloud-only".into(),
                uid: Some("10001".into()),
                name: "New cloud-only light".into(),
                model: "yeelink.light.ml9".into(),
                spec_type: Some(compiled.type_urn),
                pid: Some(0),
                token: None,
                online: Some(true),
                local_ip: None,
                parent_id: None,
            },
        ],
    };
    {
        let mut holder = restored.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        runner.catalog_refresh.set_client(Rc::new(
            CloudClient::for_test(&cloud_base, Duration::from_millis(500)).unwrap(),
        ));
        runner.sessions.catalog = Some(catalog.clone());
        restored
            .finish_catalog_refresh(
                runner,
                CatalogTaskResult::Owned {
                    snapshot: store.xiaomi().snapshot().unwrap(),
                    owned: Some(owned),
                },
            )
            .unwrap();
    }
    let restored_feature = restored
        .service()
        .features()
        .into_iter()
        .find(|candidate| candidate.identity.role == crate::device::FeatureRole::Light)
        .unwrap()
        .identity;
    {
        let holder = restored.inner.runner.borrow();
        let runner = holder.as_ref().unwrap();
        let snapshot = runner.sessions.admission.snapshot().unwrap();
        let restored_admission = snapshot
            .features
            .iter()
            .find(|candidate| candidate.identity == restored_feature)
            .unwrap();
        assert!(restored_admission.paths.cloud);
        assert!(
            RuntimeTransports::new(restored.inner.registry.clone())
                .available_paths(&restored_feature.physical)
                .cloud
        );
    }
    block_on(future::or(
        async {
            restored.inner.state.run().await;
            panic!("state runtime stopped before the historical cloud read completed")
        },
        async {
            wait_until(Duration::from_secs(1), || {
                restored
                    .service()
                    .snapshot(&restored_feature)
                    .and_then(|snapshot| snapshot.property(crate::device::Property::Power).cloned())
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
            restored.inner.state.stop();
        },
    ));
    {
        let mut holder = restored.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        restored
            .inner
            .registry
            .revoke_cloud(&restored_feature.physical);
        runner.sessions.cloud_routes.clear();
        restored
            .finish_catalog_refresh(
                runner,
                CatalogTaskResult::Resolved {
                    snapshot: store.xiaomi().snapshot().unwrap(),
                    result: Ok((catalog.catalog.clone(), catalog.specifications.clone())),
                },
            )
            .unwrap();
        assert!(
            RuntimeTransports::new(restored.inner.registry.clone())
                .available_paths(&restored_feature.physical)
                .cloud
        );
    }
}

#[test]
fn rejected_cloud_selection_retires_the_old_ack_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let runtime = XiaomiRuntime::new(store.clone()).unwrap();
    let authority = SessionAuthority::new();
    let control = CloudConnectionControl::new(BTreeSet::from(["device.did".into()]));
    let (_facts, receiver) = flume::bounded(1);
    let mut holder = runtime.inner.runner.borrow_mut();
    let runner = holder.as_mut().unwrap();
    runner.sessions.cloud_notifications = Some(ActiveCloudNotifications {
        authority,
        control,
        task: future::pending().boxed_local(),
        fact: cloud_connection_fact(receiver),
        desired: BTreeSet::from(["device.did".into()]),
        generation: 9,
        session_id: 9,
        tokens: BTreeMap::new(),
        publication: None,
    });

    runner
        .sessions
        .finish_cloud_selection(
            &runtime.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
            CloudSelectionResult {
                desired: BTreeSet::from(["device.new".into()]),
                result: Err(crate::xiaomi::mqtt::MqttError::guard_failed()),
            },
        )
        .unwrap();

    let cloud = runner.sessions.cloud_notifications.as_ref().unwrap();
    assert!(cloud.desired.is_empty());
    assert_eq!(cloud.generation, 0);
    assert_eq!(cloud.session_id, 0);
    assert!(
        runtime
            .status()
            .diagnostics
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                XiaomiRuntimeDiagnostic::Boundary(XiaomiBoundaryDiagnostic {
                    component: XiaomiRuntimeComponent::Cloud,
                    stage: XiaomiFailureStage::Subscribe,
                    code: XiaomiSafeFailureCode::Unavailable,
                    subject: None,
                })
            ))
    );
}

#[test]
fn superseded_cloud_selection_preserves_the_confirmed_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let runtime = XiaomiRuntime::new(store).unwrap();
    let authority = SessionAuthority::new();
    let control = CloudConnectionControl::new(BTreeSet::from(["device.new".into()]));
    let (_facts, receiver) = flume::bounded(1);
    let mut holder = runtime.inner.runner.borrow_mut();
    let runner = holder.as_mut().unwrap();
    runner.sessions.cloud_notifications = Some(ActiveCloudNotifications {
        authority,
        control,
        task: future::pending().boxed_local(),
        fact: cloud_connection_fact(receiver),
        desired: BTreeSet::from(["device.did".into()]),
        generation: 9,
        session_id: 9,
        tokens: BTreeMap::new(),
        publication: None,
    });

    runner
        .sessions
        .finish_cloud_selection(
            &runtime.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
            CloudSelectionResult {
                desired: BTreeSet::from(["device.did".into()]),
                result: Err(crate::xiaomi::mqtt::MqttError::superseded()),
            },
        )
        .unwrap();

    let cloud = runner.sessions.cloud_notifications.as_ref().unwrap();
    assert_eq!(cloud.desired, BTreeSet::from(["device.did".into()]));
    assert_eq!(cloud.generation, 9);
    assert_eq!(cloud.session_id, 9);
    assert!(runtime.status().diagnostics.is_empty());
}

#[test]
fn cloud_connection_lifetime_switches_the_actual_wire_allowlist_once() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (first_seen, first_ready) = flume::bounded(1);
    let (release_first, release) = flume::bounded(1);
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buffer = BytesMut::new();
        assert!(matches!(
            mqtt_packet(&mut stream, &mut buffer),
            v5::Packet::Connect(_)
        ));
        mqtt_write(
            &mut stream,
            &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
        );
        let v5::Packet::Subscribe(first) = mqtt_packet(&mut stream, &mut buffer) else {
            panic!("expected first cloud allowlist subscription")
        };
        assert_eq!(first.filters.len(), 3);
        assert!(
            first
                .filters
                .iter()
                .all(|filter| filter.path.contains("device.a"))
        );
        first_seen.send(()).unwrap();
        release.recv_timeout(Duration::from_secs(3)).unwrap();
        mqtt_write(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                first.pkid,
                vec![v5::SubscribeReasonCode::QoS2; first.filters.len()],
            )),
        );
        for _ in 0..3 {
            let v5::Packet::Unsubscribe(removed) = mqtt_packet(&mut stream, &mut buffer) else {
                panic!("expected old cloud allowlist removal")
            };
            assert_eq!(removed.filters.len(), 1);
            assert!(removed.filters[0].contains("device.a"));
            let mut unsuback = v5::UnsubAck::new(removed.pkid);
            unsuback.reasons = vec![v5::UnsubAckReason::Success];
            mqtt_write(&mut stream, &v5::Packet::UnsubAck(unsuback));
        }
        let v5::Packet::Subscribe(second) = mqtt_packet(&mut stream, &mut buffer) else {
            panic!("expected replacement cloud allowlist subscription")
        };
        assert_eq!(second.filters.len(), 3);
        assert!(
            second
                .filters
                .iter()
                .all(|filter| filter.path.contains("device.b"))
        );
        mqtt_write(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                second.pkid,
                vec![v5::SubscribeReasonCode::QoS2; second.filters.len()],
            )),
        );
        let mut closed = [0; 1];
        assert_eq!(stream.read(&mut closed).unwrap(), 0);
    });

    block_on(async {
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
        let authority = SessionAuthority::new();
        let (mqtt, session, handle, notifications) =
            crate::xiaomi::cloud::CloudNotificationSession::new(
                "oauth",
                "access",
                Duration::from_secs(5),
                &address.ip().to_string(),
                address.port(),
                None,
            )
            .unwrap();
        let live = future::or(
            async { mqtt.run().await.map_err(TransportStartupError::Mqtt) },
            async { session.run().await.map_err(TransportStartupError::Mqtt) },
        )
        .boxed_local();
        let control = CloudConnectionControl::new(BTreeSet::from(["device.a".into()]));
        let (facts, fact_receiver) = flume::bounded(8);
        let lifetime = active_cloud_connection_lifetime(
            handle,
            authority,
            notifications,
            live,
            BTreeSet::new(),
            control.clone(),
            facts,
        );
        let application = async {
            first_ready.recv_async().await.unwrap();
            control.set_desired(BTreeSet::from(["device.b".into()]));
            release_first.send_async(()).await.unwrap();
            let first = fact_receiver.recv_async().await.unwrap();
            let CloudConnectionFact::Selection(first) = first else {
                panic!("expected first cloud selection ACK")
            };
            assert_eq!(first.desired, BTreeSet::from(["device.a".into()]));
            assert!(first.result.is_ok());
            let second = fact_receiver.recv_async().await.unwrap();
            let CloudConnectionFact::Selection(second) = second else {
                panic!("expected replacement cloud selection ACK")
            };
            assert_eq!(second.desired, BTreeSet::from(["device.b".into()]));
            assert!(second.result.is_ok());
            control.stop();
        };
        let completed = future::or(
            async {
                future::zip(lifetime, application).await;
                true
            },
            async {
                Timer::after(Duration::from_secs(3)).await;
                false
            },
        )
        .await;
        assert!(completed, "cloud wire source switch did not finish");
    });
    broker.join().unwrap();
}
