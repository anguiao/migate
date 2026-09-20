use super::*;

#[test]
fn gateway_ready_from_a_disconnected_pending_attempt_cannot_be_published() {
    exercise_gateway_disconnect(false);
}

#[test]
fn active_gateway_disconnect_revokes_before_publication_cleanup() {
    exercise_gateway_disconnect(true);
}

fn exercise_gateway_disconnect(active: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (disconnect, disconnected) = std::sync::mpsc::sync_channel(1);
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
        let v5::Packet::Subscribe(initial) = mqtt_packet(&mut stream, &mut buffer) else {
            panic!("expected gateway base subscription")
        };
        mqtt_write(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                initial.pkid,
                vec![v5::SubscribeReasonCode::QoS2; initial.filters.len()],
            )),
        );
        let devices = receive_request(&mut stream, &mut buffer);
        reply(
            &mut stream,
            &mut buffer,
            41,
            devices.return_topic.as_deref().unwrap(),
            devices.mid,
            r#"{"devList":{}}"#,
        );
        disconnected.recv_timeout(Duration::from_secs(3)).unwrap();
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
                virtual_did: identity.virtual_did.clone(),
                private_key_pem: identity.private_key_pem,
                certificate_pem: certificate,
            })
            .unwrap();
        let authority = SessionAuthority::new();
        let startup_authority = authority.clone();
        let startup = async move {
            crate::xiaomi::runtime::start_gateway_plain_for_test(
                match address {
                    std::net::SocketAddr::V4(value) => value,
                    _ => unreachable!(),
                },
                MqttConfig::new("runtime", None, Duration::from_secs(5)),
                &identity.virtual_did,
                123,
                "192.0.2.10",
                NetworkEpoch::new(7),
                startup_authority.mqtt_guard(),
                Instant::now() + Duration::from_secs(2),
            )
            .await
        }
        .boxed_local();
        let network = NetworkUpdate {
            epoch: NetworkEpoch::new(7),
            snapshot: crate::xiaomi::discovery::NetworkSnapshot::select(
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
            .unwrap(),
        };
        let endpoint = GatewayEndpoint {
            interface_index: 7,
            source_address: "192.0.2.2".parse().unwrap(),
            address: "192.0.2.10".parse().unwrap(),
            port: 8883,
        };
        let interface = network.snapshot.interfaces()[0].clone();
        let control = GatewayConnectionControl::new();
        let (facts, fact_receiver) = flume::bounded(8);
        let lifetime = gateway_full_connection_lifetime(
            GatewayConnectionConfig {
                candidate: GatewayCandidate {
                    gateway_did: 123,
                    home_group: "group".into(),
                    endpoints: vec![endpoint.clone()],
                    unverified: false,
                },
                endpoints: vec![(endpoint, interface)],
                network,
                virtual_did: "unused".into(),
                private_key_pem: String::new(),
                certificate_pem: String::new(),
                authority,
                setup: ConnectionSetup::new(4),
                startup: RefCell::new(Some(startup)),
            },
            control.clone(),
            facts,
        );
        let completed = future::or(
            async {
                let application = async {
                    let mut ready = None;
                    loop {
                        match fact_receiver.recv_async().await.unwrap() {
                            GatewayConnectionFact::Ready(current) => {
                                if ready.is_none() {
                                    ready = Some(current);
                                    if active {
                                        control.publish(GatewayPublication::Active);
                                    }
                                    disconnect.send(()).unwrap();
                                }
                            }
                            GatewayConnectionFact::Disconnected { attempt } => {
                                assert_eq!(attempt, 1);
                                assert!(!ready.as_ref().unwrap().authority.check());
                                control.stop();
                                return ready;
                            }
                            _ => {}
                        }
                    }
                };
                future::or(application, async {
                    lifetime.await;
                    panic!("gateway session stopped before disconnect evidence")
                })
                .await
            },
            async {
                Timer::after(Duration::from_secs(3)).await;
                None
            },
        )
        .await;
        let Some(ready) = completed else {
            panic!("gateway attempt did not publish disconnect evidence")
        };
        assert_eq!(ready.attempt, 1);
        assert!(!ready.authority.check());
    });
    broker.join().unwrap();
}

#[test]
fn outer_runtime_admits_controls_and_applies_a_real_gateway_push() {
    let (cloud_base, cloud_requests) = crate::xiaomi::test_support::dynamic_mock_server(
        8,
        |request| {
            if request.target.ends_with("/homeroom/gethome") {
                return crate::xiaomi::test_support::MockResponse::json(401, r#"{"code":401}"#);
            }
            if request.target.contains("/oauth/get_token") {
                return crate::xiaomi::test_support::MockResponse::json(401, r#"{"code":401}"#);
            }
            let body: serde_json::Value = serde_json::from_str(&request.body)
                .unwrap_or_else(|error| panic!("unexpected {} body: {error}", request.target));
            let result = body["params"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    let piid = item["piid"].as_u64().unwrap();
                    if request.target.ends_with("/get") {
                        serde_json::json!({
                            "did":"device.did","siid":2,"piid":piid,"code":0,
                            "value": match piid { 1 => serde_json::json!(false), 2 => serde_json::json!(50), 3 => serde_json::json!(4000), _ => serde_json::json!(0) }
                        })
                    } else {
                        serde_json::json!({"did":"device.did","siid":2,"piid":piid,"code":0})
                    }
                })
                .collect::<Vec<_>>();
            crate::xiaomi::test_support::MockResponse::json(
                200,
                &serde_json::json!({"code":0,"result":result}).to_string(),
            )
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
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
        let v5::Packet::Subscribe(initial) = mqtt_packet(&mut stream, &mut buffer) else {
            panic!("expected initial subscriptions")
        };
        mqtt_write(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                initial.pkid,
                vec![v5::SubscribeReasonCode::QoS2; initial.filters.len()],
            )),
        );
        let devices = receive_request(&mut stream, &mut buffer);
        reply(
            &mut stream,
            &mut buffer,
            41,
            devices.return_topic.as_deref().unwrap(),
            devices.mid,
            r#"{"devList":{"device.did":{"name":"Light","urn":"urn:miot-spec-v2:device:light:0000A001:yeelink-ml9:1","model":"yeelink.light.ml9","online":true,"specV2Access":true,"pushAvailable":true}}}"#,
        );
        let mut pushed = false;
        let mut set_count = 0;
        while set_count == 0 {
            match mqtt_packet(&mut stream, &mut buffer) {
                v5::Packet::PingReq => {
                    stream.write_all(&[0xd0, 0x00]).unwrap();
                }
                v5::Packet::Subscribe(subscribe) => {
                    mqtt_write(
                        &mut stream,
                        &v5::Packet::SubAck(v5::SubAck::new(
                            subscribe.pkid,
                            vec![v5::SubscribeReasonCode::QoS2; subscribe.filters.len()],
                        )),
                    );
                    if !pushed {
                        let payload = crate::xiaomi::gateway::MipsEnvelope {
                            mid: 0,
                            return_topic: None,
                            payload: r#"{"did":"device.did","siid":2,"piid":1,"value":false}"#
                                .into(),
                            from: Some("local".into()),
                        }
                        .encode()
                        .unwrap();
                        let mut publish = v5::Publish::new(
                            "virtual/appMsg/notify/iot/device.did/property/2.1",
                            QoS::AtMostOnce,
                            payload,
                        );
                        publish.pkid = 0;
                        mqtt_write(&mut stream, &v5::Packet::Publish(publish));
                        pushed = true;
                    }
                }
                v5::Packet::Publish(publish) => {
                    let request =
                        crate::xiaomi::gateway::MipsEnvelope::decode(&publish.payload).unwrap();
                    mqtt_write(
                        &mut stream,
                        &v5::Packet::PubRec(v5::PubRec::new(publish.pkid)),
                    );
                    loop {
                        match mqtt_packet_ignoring_ping(&mut stream, &mut buffer) {
                            v5::Packet::PubRel(_) => break,
                            v5::Packet::Subscribe(subscribe) => {
                                mqtt_write(
                                    &mut stream,
                                    &v5::Packet::SubAck(v5::SubAck::new(
                                        subscribe.pkid,
                                        vec![
                                            v5::SubscribeReasonCode::QoS2;
                                            subscribe.filters.len()
                                        ],
                                    )),
                                );
                                if !pushed {
                                    let payload = crate::xiaomi::gateway::MipsEnvelope {
                                        mid: 0,
                                        return_topic: None,
                                        payload: r#"{"did":"device.did","siid":2,"piid":1,"value":false}"#
                                            .into(),
                                        from: Some("local".into()),
                                    }
                                    .encode()
                                    .unwrap();
                                    let mut notification = v5::Publish::new(
                                        "virtual/appMsg/notify/iot/device.did/property/2.1",
                                        QoS::AtMostOnce,
                                        payload,
                                    );
                                    notification.pkid = 0;
                                    mqtt_write(&mut stream, &v5::Packet::Publish(notification));
                                    pushed = true;
                                }
                            }
                            packet => panic!("expected PUBREL, got {packet:?}"),
                        }
                    }
                    mqtt_write(
                        &mut stream,
                        &v5::Packet::PubComp(v5::PubComp::new(publish.pkid)),
                    );
                    let value: serde_json::Value = serde_json::from_str(&request.payload).unwrap();
                    let rpc = &value["rpc"];
                    if rpc.is_null() {
                        let piid = value["piid"].as_u64().unwrap();
                        let result = serde_json::json!({
                            "value": match piid { 1 => serde_json::json!(false), 2 => serde_json::json!(50), 3 => serde_json::json!(4000), _ => serde_json::json!(0) }
                        });
                        reply(
                            &mut stream,
                            &mut buffer,
                            50 + piid as u16,
                            request.return_topic.as_deref().unwrap(),
                            request.mid,
                            &result.to_string(),
                        );
                    } else {
                        assert_eq!(rpc["method"], "set_properties");
                        assert_eq!(rpc["params"][0]["did"], "device.did");
                        assert_eq!(rpc["params"][0]["siid"], 2);
                        assert_eq!(rpc["params"][0]["piid"], 1);
                        assert_eq!(rpc["params"][0]["value"], false);
                        set_count += 1;
                        reply(
                            &mut stream,
                            &mut buffer,
                            80,
                            request.return_topic.as_deref().unwrap(),
                            request.mid,
                            r#"{"result":[{"did":"device.did","siid":2,"piid":1,"code":0}]}"#,
                        );
                    }
                }
                packet => panic!("unexpected broker packet: {packet:?}"),
            }
        }
        assert!(pushed);
        assert_eq!(set_count, 1);
        thread::sleep(Duration::from_millis(50));
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
                virtual_did: identity.virtual_did.clone(),
                private_key_pem: identity.private_key_pem,
                certificate_pem: certificate,
            })
            .unwrap();
        let runtime = XiaomiRuntime::new(store.clone()).unwrap();
        let spec = include_str!("../../../../../tests/fixtures/miot_specs/yeelink.light.ml9.json");
        let type_urn = "urn:miot-spec-v2:device:light:0000A001:yeelink-ml9:1";
        let owned = crate::xiaomi::cloud::OwnedCatalog {
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
        let specifications = HashMap::from([(type_urn.to_owned(), spec.to_owned())]);
        let catalog = crate::xiaomi::catalog::assemble_catalog(&owned, &specifications).unwrap();
        let snapshot = store.xiaomi().snapshot().unwrap();
        let network = NetworkUpdate {
            epoch: NetworkEpoch::new(9),
            snapshot: crate::xiaomi::discovery::NetworkSnapshot::select(
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
            .unwrap(),
        };
        let candidate = GatewayCandidate {
            gateway_did: 123,
            home_group: "group".into(),
            endpoints: vec![GatewayEndpoint {
                interface_index: 7,
                source_address: "192.0.2.2".parse().unwrap(),
                address: "192.0.2.10".parse().unwrap(),
                port: 8883,
            }],
            unverified: false,
        };
        let authority = SessionAuthority::new();
        {
            let mut holder = runtime.inner.runner.borrow_mut();
            let runner = holder.as_mut().unwrap();
            runner.sessions.catalog = Some(crate::xiaomi::runtime::AdmissionCatalog {
                account: crate::device::AccountId::new("10001").unwrap(),
                session_generation: snapshot.session_generation,
                catalog,
                specifications,
            });
            runner.sessions.cloud_validated = true;
            runner.catalog_refresh.set_client(Rc::new(
                CloudClient::for_test(&cloud_base, Duration::from_secs(1)).unwrap(),
            ));
            runner.auth.set_service(Rc::new(AuthService::new(
                store.xiaomi(),
                CloudClient::for_test(&cloud_base, Duration::from_secs(1)).unwrap(),
            )));
            runner.sessions.admission.invalidate(NetworkEpoch::new(9));
            runner.discovery.set_network(network.clone());
            runner.discovery.disable_monitor();
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
            let startup_authority = authority.clone();
            let startup = async move {
                crate::xiaomi::runtime::start_gateway_plain_for_test(
                    match address {
                        std::net::SocketAddr::V4(value) => value,
                        _ => unreachable!(),
                    },
                    MqttConfig::new("runtime", None, Duration::from_secs(5)),
                    &identity.virtual_did,
                    123,
                    "192.0.2.10",
                    NetworkEpoch::new(9),
                    startup_authority.mqtt_guard(),
                    Instant::now() + Duration::from_secs(3),
                )
                .await
            }
            .boxed_local();
            let endpoint = candidate.endpoints[0].clone();
            let interface = network
                .snapshot
                .interfaces_with_index(endpoint.interface_index)
                .next()
                .unwrap()
                .clone();
            let control = GatewayConnectionControl::new();
            let (fact_sender, fact_receiver) = flume::bounded(256);
            let config = GatewayConnectionConfig {
                candidate: candidate.clone(),
                endpoints: vec![(endpoint, interface)],
                network: network.clone(),
                virtual_did: "test-virtual-did".into(),
                private_key_pem: String::new(),
                certificate_pem: String::new(),
                authority: authority.clone(),
                setup: runner.sessions.gateway_setup.clone(),
                startup: RefCell::new(Some(startup)),
            };
            runner.sessions.connecting_gateways.insert(
                123,
                ConnectingGateway {
                    candidate,
                    network,
                    account: crate::device::AccountId::new("10001").unwrap(),
                    session_generation: snapshot.session_generation,
                    authority,
                    task: gateway_full_connection_lifetime(config, control.clone(), fact_sender),
                    fact: gateway_connection_fact(fact_receiver),
                    control,
                    attempt: None,
                },
            );
        }
        let service = runtime.service();
        let credential_store = store.xiaomi();
        let app_completed = Rc::new(Cell::new(false));
        let completed_by_app = app_completed.clone();
        let app = async {
            let feature = loop {
                if let Some(feature) = service
                    .features()
                    .into_iter()
                    .find(|feature| feature.identity.role == crate::device::FeatureRole::Light)
                    && service.is_available(&feature.identity)
                {
                    break feature.identity;
                }
                Timer::after(Duration::from_millis(10)).await;
            };
            loop {
                if service
                    .snapshot(&feature)
                    .and_then(|snapshot| snapshot.property(crate::device::Property::Power).cloned())
                    .is_some_and(|state| {
                        matches!(
                            state,
                            crate::device::PropertyState::Current {
                                value: crate::device::PropertyValue::Power(false),
                                ..
                            }
                        )
                    })
                {
                    break;
                }
                Timer::after(Duration::from_millis(10)).await;
            }
            let ticket = service.command(&feature, crate::device::DeviceCommand::SetPower(false));
            assert_eq!(
                ticket.await,
                crate::device::CommandOutcome::Accepted,
                "token recovery status: {:?}",
                runtime.status()
            );
            runtime.refresh();
            loop {
                if matches!(
                    runtime.auth_report().authentication,
                    crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                ) {
                    break;
                }
                Timer::after(Duration::from_millis(10)).await;
            }
            credential_store
                .update_tokens(&crate::storage::TokenSet {
                    access_token: "new-access".into(),
                    refresh_token: "new-refresh".into(),
                    expires_at: unix_time() + 30 * 24 * 60 * 60,
                    refresh_at: unix_time() + 20 * 24 * 60 * 60,
                })
                .unwrap();
            Timer::after(Duration::from_millis(250)).await;
            let ticket = service.command(&feature, crate::device::DeviceCommand::SetPower(true));
            assert_eq!(
                ticket.await,
                crate::device::CommandOutcome::Accepted,
                "token recovery status: {:?}",
                runtime.status()
            );
            assert!(
                service
                    .snapshot(&feature)
                    .and_then(|snapshot| snapshot.property(crate::device::Property::Power).cloned())
                    .is_some_and(|state| matches!(
                        state,
                        crate::device::PropertyState::Current {
                            value: crate::device::PropertyValue::Power(false),
                            ..
                        }
                    ))
            );
            completed_by_app.set(true);
            runtime.stop();
        };
        let completed = future::or(
            async {
                future::or(
                    async {
                        runtime.run().await.unwrap();
                    },
                    app,
                )
                .await;
                true
            },
            async {
                Timer::after(Duration::from_secs(5)).await;
                false
            },
        )
        .await;
        assert!(
            completed,
            "outer Xiaomi runtime scenario timed out: {:?}",
            runtime.status()
        );
        assert!(
            app_completed.get(),
            "runtime exited before application assertions completed"
        );
    });
    broker.join().unwrap();
    let request = loop {
        let request = cloud_requests.recv_timeout(Duration::from_secs(1)).unwrap();
        if request.target.ends_with("/set") {
            break request;
        }
    };
    assert_eq!(request.target, "/app/v2/miotspec/prop/set");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearernew-access")
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&request.body).unwrap()["params"][0]["value"],
        true
    );
    assert!(
        cloud_requests
            .try_iter()
            .all(|request| !request.target.ends_with("/set"))
    );
}
