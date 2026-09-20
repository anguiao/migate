use super::*;
use crate::{
    device::{AccountId, CommandOutcome, DeviceDid, DeviceService, FeatureIdentity, HomeId},
    storage::{Store, TokenSet, XiaomiRecord},
    xiaomi::{
        catalog::compile_spec,
        test_support::{MockResponse, mock_server},
    },
};
use std::cell::Cell;

fn record() -> XiaomiRecord {
    XiaomiRecord {
        uid: "10001".into(),
        region: "cn".into(),
        oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
        redirect_uri: "http://127.0.0.1/callback".into(),
        tokens: TokenSet {
            access_token: "access-token".into(),
            refresh_token: "refresh-token".into(),
            expires_at: 2_000_000_000,
            refresh_at: 1_900_000_000,
        },
        virtual_did: "123456789012345".into(),
        private_key_pem: "test-key".into(),
        certificate_pem: "test-certificate".into(),
    }
}

fn light() -> (FeatureIdentity, FeatureDescriptor) {
    let document = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/miot_specs/yeelink.light.ml9.json"
    ))
    .unwrap();
    let descriptor = compile_spec("yeelink.light.ml9", &document)
        .unwrap()
        .features
        .remove(0);
    let identity = FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("10001").unwrap(),
            home: HomeId::new("home-a").unwrap(),
            parent_did: DeviceDid::new("1234").unwrap(),
        },
        service_instance: descriptor.definition.service_instance,
        role: descriptor.definition.role,
    };
    (identity, descriptor)
}

fn run_cloud_command(
    response: MockResponse,
    change_account_after_delivery: bool,
) -> (
    CommandOutcome,
    Vec<crate::xiaomi::test_support::ReceivedRequest>,
    bool,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&record()).unwrap();
    let snapshot = store.xiaomi().snapshot().unwrap();
    let (base, requests) = if change_account_after_delivery {
        let path = directory.path().to_owned();
        crate::xiaomi::test_support::dynamic_mock_server(1, move |_| {
            let other = Store::open(&path).unwrap();
            other.xiaomi().logout().unwrap();
            other.xiaomi().replace(&record()).unwrap();
            response.clone()
        })
    } else {
        mock_server(vec![response])
    };
    let (identity, descriptor) = light();
    let service = DeviceService::new();
    service.publish(
        identity.clone(),
        "Transport light",
        descriptor.definition.capabilities.clone(),
    );
    service.set_state_availability(&identity, true);
    let registry = CurrentSessionRegistry::default();
    let authority = SessionAuthority::new();
    let cloud = Rc::new(CloudClient::for_test(&base, Duration::from_millis(300)).unwrap());
    registry.install_cloud_if_changed(
        identity.physical.clone(),
        cloud.clone(),
        "access-token".into(),
        i64::MAX,
        authority.clone(),
    );
    let mut sibling = identity.physical.clone();
    sibling.parent_did = DeviceDid::new("5678").unwrap();
    registry.install_cloud_if_changed(
        sibling.clone(),
        cloud,
        "access-token".into(),
        i64::MAX,
        authority,
    );
    let transports = Rc::new(RuntimeTransports::new(registry));
    let runtime = super::super::CommandRuntime::with_limits(
        service.clone(),
        transports.clone(),
        super::super::CommandLimits {
            total: Duration::from_millis(500),
            cloud_attempt: Duration::from_millis(300),
            ..super::super::CommandLimits::default()
        },
    );
    runtime.register(super::super::RuntimeFeature {
        identity: identity.clone(),
        descriptor,
        authority_generation: 1,
        auth_session_generation: snapshot.session_generation,
    });
    let ticket = service.command(&identity, DeviceCommand::SetPower(false));
    async_io::block_on(runtime.run_until_idle());
    let outcome = async_io::block_on(ticket);
    let mut received = Vec::new();
    while let Ok(request) = requests.recv_timeout(Duration::from_millis(50)) {
        received.push(request);
    }
    let sibling_cloud_available = transports.available_paths(&sibling).cloud;
    (outcome, received, sibling_cloud_available)
}

#[test]
fn actual_cloud_adapter_sends_one_exact_property_and_classifies_http_ambiguity() {
    let accepted = MockResponse::json(
        200,
        r#"{"code":0,"result":[{"did":"1234","siid":2,"piid":1,"code":0}]}"#,
    );
    let (outcome, request, _) = run_cloud_command(accepted, false);
    assert_eq!(outcome, CommandOutcome::Accepted);
    assert_eq!(request.len(), 1);
    let request = &request[0];
    assert_eq!(request.target, "/app/v2/miotspec/prop/set");
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"params":[{"did":"1234","siid":2,"piid":1,"value":false}]})
    );

    let (outcome, request, _) = run_cloud_command(MockResponse::json(503, "unavailable"), false);
    assert_eq!(outcome, CommandOutcome::Ambiguous);
    assert_eq!(request.len(), 1);
}

#[test]
fn current_cloud_unauthorized_response_closes_every_route_sharing_the_token() {
    let (outcome, requests, sibling_cloud_available) =
        run_cloud_command(MockResponse::json(401, "unauthorized"), false);

    assert_eq!(outcome, CommandOutcome::Rejected(401));
    assert_eq!(requests.len(), 1);
    assert!(
        !sibling_cloud_available,
        "a known-invalid credential must close all routes sharing its lease immediately"
    );
}

#[test]
fn dropping_a_sent_lan_read_at_its_state_deadline_revokes_the_exact_route() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&record()).unwrap();
    let (identity, _) = light();
    let authority = SessionAuthority::new();
    let registry = CurrentSessionRegistry::default();
    let (session, handle, _notifications, device, token) =
        crate::xiaomi::lan::session_pair("lumi.acpartner.mcn02");
    let mut session = session.run().boxed_local();
    let sent = Rc::new(Cell::new(false));
    let observed_sent = sent.clone();
    async_io::block_on(futures_lite::future::or(
        async {
            futures_lite::future::zip(
                handle.authenticate(
                    LanProperty { siid: 2, piid: 1 },
                    Instant::now() + Duration::from_secs(1),
                    LanSendGuard::new(),
                ),
                crate::xiaomi::lan::reject_native_authentication_probe(&device, &token),
            )
            .await
            .0
            .unwrap();
        },
        async {
            let stopped = session.as_mut().await;
            panic!("LAN session stopped during authentication: {stopped:?}")
        },
    ));
    registry.install_lan(identity.physical.clone(), handle, authority, None);
    let transport = RuntimeTransports::new(registry.clone());
    let request = StateReadRequest {
        device: identity.physical.clone(),
        path: ControlPath::Lan,
        targets: vec![ReadTarget { siid: 2, piid: 1 }],
    };
    let guard = StateReadGuard::for_test(Instant::now() + Duration::from_millis(40));
    async_io::block_on(futures_lite::future::or(
        async {
            futures_lite::future::race(
                async {
                    let _ = transport
                        .read(request, Duration::from_millis(200), guard)
                        .await;
                },
                async {
                    async_io::Timer::after(Duration::from_millis(70)).await;
                },
            )
            .await;
        },
        futures_lite::future::or(
            async {
                let stopped = session.as_mut().await;
                panic!("LAN session stopped during pending read: {stopped:?}")
            },
            async move {
                let (request, _) =
                    crate::xiaomi::lan::receive_request_for_test(&device, &token).await;
                assert_eq!(request["method"], "get_properties");
                observed_sent.set(true);
                futures_lite::future::pending::<()>().await;
            },
        ),
    ));
    assert!(sent.get(), "the LAN read did not reach the UDP boundary");
    let failures = registry.drain_route_failures();
    assert!(
        !transport.available_paths(&identity.physical).lan,
        "sent read retained its route, failures={}",
        failures.len()
    );
    assert_eq!(failures.len(), 1);
}

#[test]
fn delivered_http_command_keeps_its_actual_reply_after_account_change() {
    let response = MockResponse::json(
        200,
        r#"{"code":0,"result":[{"did":"1234","siid":2,"piid":1,"code":0}]}"#,
    );
    let (outcome, request, _) = run_cloud_command(response, true);
    assert_eq!(outcome, CommandOutcome::Accepted);
    assert_eq!(request.len(), 1);
}

#[test]
fn legacy_normalization_uses_the_descriptor_command_codec() {
    let document = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/miot_specs/lumi.acpartner.mcn02.json"
    ))
    .unwrap();
    let feature = compile_spec("lumi.acpartner.mcn02", &document)
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == crate::device::FeatureRole::Climate)
        .unwrap();
    for (property, value) in [
        (Property::Power, PropertyValue::Power(true)),
        (
            Property::HvacMode,
            PropertyValue::HvacMode(crate::device::HvacMode::Cool),
        ),
        (
            Property::TargetTemperature,
            PropertyValue::Temperature(24.0),
        ),
        (Property::FanSpeed, PropertyValue::FanSpeed(2)),
        (
            Property::SwingMode,
            PropertyValue::SwingMode(crate::device::SwingMode::Vertical),
        ),
    ] {
        let mapping = feature
            .binding
            .properties
            .iter()
            .find(|mapping| mapping.property == property)
            .unwrap();
        assert!(
            normalize_legacy(
                &feature,
                ReadTarget {
                    siid: mapping.siid,
                    piid: mapping.piid,
                },
                property,
                &value,
            )
            .is_some()
        );
    }
}

#[test]
fn stale_cloud_unauthorized_lease_cannot_revoke_replacement_token() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&record()).unwrap();
    let (identity, _) = light();
    let authority = SessionAuthority::new();
    let registry = CurrentSessionRegistry::default();
    let client =
        Rc::new(CloudClient::for_test("http://127.0.0.1:9", Duration::from_millis(20)).unwrap());
    registry.install_cloud_if_changed(
        identity.physical.clone(),
        client.clone(),
        "old-token".into(),
        i64::MAX,
        authority.clone(),
    );
    let mut second = identity.physical.clone();
    second.parent_did = DeviceDid::new("5678").unwrap();
    registry.install_cloud_if_changed(
        second.clone(),
        client.clone(),
        "old-token".into(),
        i64::MAX,
        authority.clone(),
    );
    let old_lease = registry.routes.borrow()[&identity.physical]
        .cloud
        .as_ref()
        .unwrap()
        .authority
        .lease();
    registry.routes.borrow()[&identity.physical]
        .cloud
        .as_ref()
        .unwrap()
        .credential_active
        .store(false, Ordering::Release);
    assert!(
        !registry.routes.borrow()[&second]
            .cloud
            .as_ref()
            .unwrap()
            .credential_active
            .load(Ordering::Acquire)
    );
    registry.install_cloud_if_changed(
        identity.physical.clone(),
        client.clone(),
        "old-token".into(),
        i64::MAX,
        authority.clone(),
    );
    let mut third = identity.physical.clone();
    third.parent_did = DeviceDid::new("9012").unwrap();
    registry.install_cloud_if_changed(
        third.clone(),
        client.clone(),
        "old-token".into(),
        i64::MAX,
        authority.clone(),
    );
    for device in [&identity.physical, &second, &third] {
        assert!(
            !registry.routes.borrow()[device]
                .cloud
                .as_ref()
                .unwrap()
                .credential_active
                .load(Ordering::Acquire),
            "reconciling the rejected credential must not reopen it"
        );
    }

    registry.revoke_device(&identity.physical);
    registry.revoke_device(&second);
    registry.revoke_device(&third);
    registry.install_cloud_if_changed(
        third.clone(),
        client.clone(),
        "old-token".into(),
        i64::MAX,
        authority.clone(),
    );
    assert!(
        !registry.routes.borrow()[&third]
            .cloud
            .as_ref()
            .unwrap()
            .credential_active
            .load(Ordering::Acquire),
        "removing every device route must not revive a rejected credential"
    );
    registry.install_cloud_if_changed(
        identity.physical.clone(),
        client,
        "new-token".into(),
        i64::MAX,
        authority,
    );

    assert!(!registry.revoke_cloud_if_authority(&identity.physical, &old_lease));
    assert!(
        registry.routes.borrow()[&identity.physical]
            .cloud
            .as_ref()
            .unwrap()
            .authority
            .check()
    );
}

#[test]
fn expired_credentials_are_rejected_from_memory_without_a_storage_check() {
    let (identity, _) = light();
    let registry = CurrentSessionRegistry::default();
    let client =
        Rc::new(CloudClient::for_test("http://127.0.0.1:9", Duration::from_millis(20)).unwrap());
    registry.install_cloud_if_changed(
        identity.physical.clone(),
        client,
        "expired-token".into(),
        unix_time() - 1,
        SessionAuthority::new(),
    );
    let transports = RuntimeTransports::new(registry);

    assert!(!transports.available_paths(&identity.physical).cloud);
    assert!(!SessionAuthority::new_until(unix_time() - 1).check());
}

#[test]
fn removing_one_cloud_device_keeps_a_sibling_route_on_the_same_credential_live() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&record()).unwrap();
    let snapshot = store.xiaomi().snapshot().unwrap();
    let (identity, descriptor) = light();
    let authority = SessionAuthority::new();
    let registry = CurrentSessionRegistry::default();
    let (base, requests) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":[{"did":"5678","siid":2,"piid":1,"code":0}]}"#,
    )]);
    let client = Rc::new(CloudClient::for_test(&base, Duration::from_millis(200)).unwrap());
    registry.install_cloud_if_changed(
        identity.physical.clone(),
        client.clone(),
        "token".into(),
        i64::MAX,
        authority.clone(),
    );
    let mut sibling = identity.physical.clone();
    sibling.parent_did = DeviceDid::new("5678").unwrap();
    registry.install_cloud_if_changed(sibling.clone(), client, "token".into(), i64::MAX, authority);

    registry.revoke_cloud(&identity.physical);

    let transports = Rc::new(RuntimeTransports::new(registry));
    assert!(!transports.available_paths(&identity.physical).cloud);
    assert!(transports.available_paths(&sibling).cloud);
    let sibling_identity = FeatureIdentity {
        physical: sibling,
        service_instance: identity.service_instance,
        role: identity.role,
    };
    let service = DeviceService::new();
    service.publish(
        sibling_identity.clone(),
        "Sibling",
        descriptor.definition.capabilities.clone(),
    );
    service.set_state_availability(&sibling_identity, true);
    let commands = super::super::CommandRuntime::new(service.clone(), transports);
    commands.register(super::super::RuntimeFeature {
        identity: sibling_identity.clone(),
        descriptor,
        authority_generation: 1,
        auth_session_generation: snapshot.session_generation,
    });
    let ticket = service.command(&sibling_identity, DeviceCommand::SetPower(false));
    async_io::block_on(commands.run_until_idle());
    assert_eq!(async_io::block_on(ticket), CommandOutcome::Accepted);
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .target,
        "/app/v2/miotspec/prop/set"
    );
}

#[test]
fn a_dead_gateway_session_selects_cloud_before_the_runtime_drains_disconnect() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&record()).unwrap();
    let snapshot = store.xiaomi().snapshot().unwrap();
    let (identity, descriptor) = light();
    let root = SessionAuthority::new();
    let gateway_authority = root.fresh_lease();
    let (_connection, mqtt, messages) = crate::xiaomi::mqtt::MqttConnection::new(
        crate::xiaomi::mqtt::MqttConfig::new("dead-gateway-test", None, Duration::from_secs(60))
            .with_endpoint("127.0.0.1", 9),
    )
    .unwrap();
    let (_gateway, gateway_handle, _notifications) = crate::xiaomi::gateway::GatewaySession::new(
        "dead-gateway-test",
        1,
        "1",
        crate::xiaomi::discovery::NetworkEpoch::new(7),
        mqtt,
        messages,
    )
    .unwrap();
    let (base, requests) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":[{"did":"1234","siid":2,"piid":1,"code":0}]}"#,
    )]);
    let registry = CurrentSessionRegistry::default();
    registry.install_gateway(
        identity.physical.clone(),
        gateway_handle,
        gateway_authority.clone(),
    );
    registry.install_cloud_if_changed(
        identity.physical.clone(),
        Rc::new(CloudClient::for_test(&base, Duration::from_millis(200)).unwrap()),
        "access-token".into(),
        i64::MAX,
        root,
    );
    gateway_authority.revoke();

    let service = DeviceService::new();
    service.publish(
        identity.clone(),
        "Fallback light",
        descriptor.definition.capabilities.clone(),
    );
    service.set_state_availability(&identity, true);
    let transports = Rc::new(RuntimeTransports::new(registry));
    assert_eq!(
        transports.available_paths(&identity.physical),
        super::super::OperationPaths {
            gateway: false,
            lan: false,
            cloud: true,
        }
    );
    let commands = super::super::CommandRuntime::new(service.clone(), transports);
    commands.register(super::super::RuntimeFeature {
        identity: identity.clone(),
        descriptor,
        authority_generation: 1,
        auth_session_generation: snapshot.session_generation,
    });
    let ticket = service.command(&identity, DeviceCommand::SetPower(false));
    async_io::block_on(commands.run_until_idle());
    assert_eq!(async_io::block_on(ticket), CommandOutcome::Accepted);
    let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(request.target.ends_with("/miotspec/prop/set"));
    assert!(requests.try_recv().is_err());
}
