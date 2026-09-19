use futures_lite::{future::block_on, io::Cursor};
use futures_util::future::LocalBoxFuture;
use migate::{
    device::{
        AccountId, CommandOutcome, DeviceCommand, DeviceCommandSink, DeviceDid, DeviceService,
        FeatureCapabilities, FeatureIdentity, FeatureRole, HomeId, PhysicalDeviceId, Property,
        PropertyValue, StateReport, StateSource,
    },
    storage::{
        DeviceRecord, PublishedFeatureDefinition, PublishedTopologyDelta, Store, TokenSet,
        XiaomiRecord,
    },
    terminal::bridge::{BridgeCommandError, handle_line, run_input},
    xiaomi::runtime::XiaomiRuntime,
};
use std::{
    cell::RefCell,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

type RecordedCalls = Rc<RefCell<Vec<(FeatureIdentity, Vec<DeviceCommand>)>>>;

struct AcceptedSink(RecordedCalls);
impl DeviceCommandSink for AcceptedSink {
    fn submit(
        &self,
        feature: FeatureIdentity,
        commands: Vec<DeviceCommand>,
    ) -> LocalBoxFuture<'static, CommandOutcome> {
        self.0.borrow_mut().push((feature, commands));
        Box::pin(async { CommandOutcome::Accepted })
    }
    fn stop_adjustment(&self, _feature: &FeatureIdentity, _property: Property) {}
}

struct Harness {
    _directory: tempfile::TempDir,
    store: Store,
    runtime: XiaomiRuntime,
    service: DeviceService,
    feature: FeatureIdentity,
    public_id: String,
    calls: RecordedCalls,
}

fn harness() -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let runtime = XiaomiRuntime::new(store.clone()).unwrap();
    let service = runtime.service();
    let feature = FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("account").unwrap(),
            home: HomeId::new("home").unwrap(),
            parent_did: DeviceDid::new("did").unwrap(),
        },
        service_instance: 2,
        role: FeatureRole::Light,
    };
    store
        .xiaomi()
        .replace(&XiaomiRecord {
            uid: "account".into(),
            region: "cn".into(),
            oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
            redirect_uri:
                "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                    .into(),
            tokens: TokenSet {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: 2_000_000_000,
                refresh_at: 1_900_000_000,
            },
            virtual_did: "123456789012345".into(),
            private_key_pem: "key".into(),
            certificate_pem: "certificate".into(),
        })
        .unwrap();
    let generation = store.xiaomi().snapshot().unwrap().session_generation;
    let allocation = store
        .devices()
        .publish_topology(&PublishedTopologyDelta {
            account: feature.physical.account.clone(),
            session_generation: generation,
            binding: None,
            devices: vec![DeviceRecord {
                identity: feature.physical.clone(),
                model: "yeelink.light.ml9".into(),
                name: "Desk light".into(),
                room_id: None,
                admitted: true,
            }],
            definitions: vec![PublishedFeatureDefinition {
                feature: feature.clone(),
                model: "yeelink.light.ml9".into(),
                spec_document: "{}".into(),
                name: "Desk light".into(),
            }],
            deactivate: Vec::new(),
        })
        .unwrap()
        .unwrap()
        .remove(0);
    service.publish(
        feature.clone(),
        "Desk light",
        FeatureCapabilities::light(true, true),
    );
    service.set_state_availability(&feature, true);
    let calls = Rc::new(RefCell::new(Vec::new()));
    service.set_command_sink(Rc::new(AcceptedSink(calls.clone())));
    Harness {
        _directory: directory,
        store,
        runtime,
        service,
        feature,
        public_id: allocation.public_id.to_string(),
        calls,
    }
}

fn line(h: &Harness, input: &str) -> Result<Option<String>, BridgeCommandError> {
    block_on(handle_line(
        &h.service,
        &h.store.devices(),
        &h.runtime,
        "/bin/migate".as_ref(),
        h._directory.path(),
        input,
        0,
    ))
}

#[test]
fn commands_require_stable_ids_and_validate_values_before_dispatch() {
    let h = harness();
    assert_eq!(line(&h, "on").unwrap_err().to_string(), "Usage: on <id>");
    assert!(
        line(&h, "on missing")
            .unwrap_err()
            .to_string()
            .contains("Unknown feature id")
    );
    assert!(line(&h, &format!("set {} brightness-percent NaN", h.public_id)).is_err());
    assert!(line(&h, &format!("set {} brightness-percent 101", h.public_id)).is_err());
    assert!(line(&h, &format!("on {} trailing", h.public_id)).is_err());
    assert!(h.calls.borrow().is_empty());
    assert!(
        line(&h, &format!("on {}", h.public_id))
            .unwrap()
            .unwrap()
            .starts_with("Accepted:")
    );
    assert_eq!(
        h.calls.borrow().as_slice(),
        [(h.feature.clone(), vec![DeviceCommand::SetPower(true)])]
    );
}

#[test]
fn status_distinguishes_current_last_known_and_unknown() {
    let h = harness();
    h.service.apply_report(StateReport::new(
        h.feature.clone(),
        1,
        StateSource::Gateway,
        10,
        [(Property::Power, PropertyValue::Power(true))],
    ));
    let current = line(&h, &format!("status {}", h.public_id))
        .unwrap()
        .unwrap();
    assert!(current.contains("Current"), "{current}");
    h.service.mark_unconfirmed(&h.feature);
    let last_known = line(&h, &format!("status {}", h.public_id))
        .unwrap()
        .unwrap();
    assert!(
        last_known.contains("LastKnown cached value"),
        "{last_known}"
    );
    assert!(last_known.contains("not current"), "{last_known}");
    h.service.apply_unknown(&h.feature, Property::Power, 2);
    let unknown = line(&h, &format!("status {}", h.public_id))
        .unwrap()
        .unwrap();
    assert!(unknown.contains("Unknown"), "{unknown}");
}

#[test]
fn help_reads_current_runtime_auth_and_devices_use_public_ids() {
    let h = harness();
    let help = line(&h, "help").unwrap().unwrap();
    assert!(help.contains("Xiaomi: not signed in (cn)."), "{help}");
    assert!(help.contains("status <id>"), "{help}");
    let devices = line(&h, "devices").unwrap().unwrap();
    assert!(devices.contains(&h.public_id), "{devices}");
    assert!(!devices.contains("virtual-light"), "{devices}");
    let status = line(&h, &format!("status {}", h.public_id))
        .unwrap()
        .unwrap();
    assert!(
        status.contains("set brightness-percent 0..100 step 1"),
        "{status}"
    );
    assert!(
        status.contains("set color-temperature-kelvin 2000..6500 step 1"),
        "{status}"
    );
}

struct FailingInput;
impl futures_lite::io::AsyncRead for FailingInput {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "input failed",
        )))
    }
}

#[test]
fn eof_returns_and_output_failures_are_propagated() {
    let h = harness();
    let mut output = Vec::new();
    block_on(run_input(
        &h.service,
        &h.store.devices(),
        &h.runtime,
        "/bin/migate".as_ref(),
        h._directory.path(),
        Cursor::new(b"help\n"),
        &mut output,
    ))
    .unwrap();
    assert!(String::from_utf8(output).unwrap().contains("Commands:"));
    let mut bytes = [0_u8; 1];
    let mut output = Cursor::new(&mut bytes[..]);
    let error = block_on(run_input(
        &h.service,
        &h.store.devices(),
        &h.runtime,
        "/bin/migate".as_ref(),
        h._directory.path(),
        Cursor::new(b"help\n"),
        &mut output,
    ))
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::WriteZero
    );
    let error = block_on(run_input(
        &h.service,
        &h.store.devices(),
        &h.runtime,
        "/bin/migate".as_ref(),
        h._directory.path(),
        futures_lite::io::BufReader::new(FailingInput),
        Vec::new(),
    ))
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::BrokenPipe
    );
}

#[test]
fn storage_failures_are_fatal_instead_of_becoming_terminal_text() {
    let h = harness();
    rusqlite::Connection::open(h.store.path())
        .unwrap()
        .execute(
            "ALTER TABLE feature_identities RENAME TO unavailable_features",
            [],
        )
        .unwrap();
    let error = block_on(run_input(
        &h.service,
        &h.store.devices(),
        &h.runtime,
        "/bin/migate".as_ref(),
        h._directory.path(),
        Cursor::new(b"devices\n"),
        Vec::new(),
    ))
    .unwrap_err();
    let storage = error
        .downcast_ref::<migate::storage::StorageError>()
        .unwrap();
    assert_eq!(storage.path(), h.store.path());
    assert_eq!(storage.operation(), "load feature identities");
}
