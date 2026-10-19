use super::{outlet, status};
use crate::{
    app::App, configuration::Revocation, storage::Store, terminal::bridge::Context,
    xiaomi::adapter::tests::app_source::AppSource,
};
use futures_lite::future::block_on;
use std::rc::Rc;

#[test]
fn startup_revocation_overlay_keeps_committed_versions_for_the_removal_plan() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let initial = AppSource::new(&store, Some(outlet()));
    let key = outlet().key().clone();
    let before = store.devices().load_configuration().unwrap();
    let identity = before.device(&key).unwrap();
    drop(initial);
    store
        .devices()
        .record_revocations(&[Revocation {
            key: key.clone(),
            reason: "confirmed removal".into(),
        }])
        .unwrap();
    let restored = AppSource::new(&store, None);
    assert!(restored.physical.is_none());
    let app = Rc::new(App::open(restored.handle.clone(), store.clone()).unwrap());
    assert!(app.frozen().is_empty());
    let plan = app.plan().unwrap().unwrap();
    assert!(plan.rebuild);
    assert_eq!(plan.configuration.root_version, before.root_version + 1);
    let withdrawn = plan.configuration.device(&key).unwrap();
    assert_eq!(
        withdrawn.configuration_version,
        identity.configuration_version + 1
    );
    assert_eq!(withdrawn.unique_id, identity.unique_id);
    assert_eq!(withdrawn.endpoint, identity.endpoint);
    assert!(withdrawn.native.is_none());
    let prepared = app.prepare(plan);
    app.apply(prepared).unwrap();
    assert!(app.frozen().is_empty());
    let saved = store.devices().load_configuration().unwrap();
    assert_eq!(saved, app.current().configuration);
    assert_eq!(store.devices().load_revocations().unwrap().len(), 1);
    let status = status(&store);
    let terminal = Context::committed(app, &status);
    let output = block_on(terminal.handle_line("status", 0))
        .unwrap()
        .unwrap();
    assert!(
        output.contains(&format!("root version {}", saved.root_version)),
        "{output}"
    );
    assert!(
        !block_on(terminal.handle_line("list", 0))
            .unwrap()
            .unwrap()
            .contains("Test Outlet")
    );
}
