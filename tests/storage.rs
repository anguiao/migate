use migate::{config::Config, device::Command, storage::Store, virtual_device::VirtualLight};
use std::fs;

#[test]
fn initializes_and_restores_identity_without_power() {
    let root = tempfile::tempdir().unwrap();
    for dir in [root.path().join("new"), root.path().join("empty")] {
        if dir.ends_with("empty") {
            fs::create_dir(&dir).unwrap();
        }
        let store = Store::open(&dir).unwrap();
        let identity = store.identity().clone();
        assert_eq!(identity.bridge_id.len(), 32);
        assert_ne!(identity.bridge_id, identity.light_id);
        let light = VirtualLight::new();
        light.execute(Command::On);
        drop(store);
        assert_eq!(Store::open(&dir).unwrap().identity(), &identity);
        assert!(!VirtualLight::new().snapshot().power);
    }
    assert_ne!(
        Store::open(root.path().join("new")).unwrap().identity(),
        Store::open(root.path().join("empty")).unwrap().identity()
    );
}

#[test]
fn raw_blobs_are_durable_on_each_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let identity = store.identity().clone();
    let blob = [0, 255, 42, 128];
    store.store(7, &blob).unwrap();
    assert_eq!(
        Store::open(dir.path()).unwrap().load(7),
        Some(blob.as_slice())
    );
    assert_eq!(store.load(8), None);
    store.remove(8).unwrap();
    store.remove(7).unwrap();
    fs::write(dir.path().join(".tmp-interrupted"), b"uncommitted").unwrap();
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.load(7), None);
    assert_eq!(reopened.identity(), &identity);
    store.flush().unwrap();
}

#[test]
fn corruption_and_partial_initialization_are_preserved() {
    for contents in [b"not json".as_slice(), br#"{}"#] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, contents).unwrap();
        let error = Store::open(dir.path()).err().unwrap().to_string();
        assert!(error.contains(path.to_str().unwrap()));
        assert_eq!(fs::read(path).unwrap(), contents);
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("leftover"), b"secret").unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert!(!dir.path().join("state.json").exists());
    assert_eq!(fs::read(dir.path().join("leftover")).unwrap(), b"secret");
}

#[test]
fn valid_json_corruption_is_detected() {
    for field in ["version", "identity", "blobs"] {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path())
            .unwrap()
            .store(7, b"secret")
            .unwrap();
        let path = dir.path().join("state.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match field {
            "version" => value["payload"][field] = 99.into(),
            "identity" => value["payload"][field]["bridge_id"] = "invalid".into(),
            _ => value["payload"][field] = serde_json::json!({}),
        }
        let damaged = serde_json::to_vec(&value).unwrap();
        fs::write(&path, &damaged).unwrap();
        assert!(Store::open(dir.path()).is_err());
        assert_eq!(fs::read(path).unwrap(), damaged);
    }
}

#[test]
fn uses_final_config_directory_and_reports_io_failure() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::parse(
        ["--data-dir".into(), "selected".into()],
        Some("ignored".into()),
        Some("unused".into()),
        root.path(),
    )
    .unwrap();
    let mut store = Store::open(&config.data_dir).unwrap();
    store.store(1, b"old").unwrap();
    assert!(!root.path().join("ignored").exists());
    let path = config.data_dir.join("state.json");
    fs::rename(&path, config.data_dir.join("saved.json")).unwrap();
    fs::create_dir(&path).unwrap();
    let error = store.store(1, b"SECRET CONTENT").unwrap_err().to_string();
    assert!(error.contains(path.to_str().unwrap()));
    assert!(!error.contains("SECRET CONTENT"));
    assert_eq!(store.load(1), Some(b"old".as_slice()));
    assert!(store.remove(1).is_err());
    assert_eq!(store.load(1), Some(b"old".as_slice()));
    assert!(store.flush().is_err());
}

#[cfg(unix)]
#[test]
fn private_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("private");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    Store::open(&dir).unwrap().store(1, b"secret").unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn malformed_credential_errors_do_not_quote_contents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    fs::write(&path, br#"{"payload":"SECRET CONTENT"}"#).unwrap();
    let error = Store::open(dir.path()).err().unwrap();
    assert!(!format!("{error:?} {error}").contains("SECRET CONTENT"));
}
