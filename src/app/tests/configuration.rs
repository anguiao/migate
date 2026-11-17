use super::{outlet, status};
use crate::{
    app::App, configuration::Revocation, storage::Store, terminal::bridge::Context,
    xiaomi::adapter::tests::app_source::AppSource,
};
use futures_lite::future::block_on;
use std::rc::Rc;

#[test]
fn source_install_shares_configuration_and_graph_before_app_updates_its_view() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let source = AppSource::new(&store, Some(outlet()));
    let app = App::open(source.handle.clone(), store.clone()).unwrap();
    let before = app.current();
    source.directory_name(outlet().key(), "Updated catalog name");
    let plan = app.plan().unwrap().unwrap();
    let prepared = source.handle.prepare(&plan);
    store
        .devices()
        .commit_configuration(&plan.configuration, &plan.clear_revocations)
        .unwrap();
    let configuration = Rc::new(plan.configuration);

    source
        .handle
        .install(&configuration, &plan.derived, &plan.operations, prepared);

    assert!(Rc::ptr_eq(&app.current(), &before));
    assert!(Rc::ptr_eq(&source.handle.configuration(), &configuration));
    assert!(Rc::ptr_eq(&source.handle.committed_graph(), &plan.derived));
}

#[test]
fn startup_revocation_overlay_keeps_committed_versions_for_the_removal_plan() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let initial = AppSource::new(&store, Some(outlet()));
    let key = outlet().key().clone();
    let before = store.devices().load_configuration().unwrap();
    let identity = before.device(&key).unwrap();
    let subscription = crate::matter::subscription_keys().start;
    store
        .matter()
        .put(subscription, b"home subscription")
        .unwrap();
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
    assert!(app.matter_configuration().entries.is_empty());
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
    assert!(app.matter_configuration().entries.is_empty());
    let saved = store.devices().load_configuration().unwrap();
    assert_eq!(saved, *app.current().configuration);
    assert_eq!(store.devices().load_revocations().unwrap().len(), 1);
    assert_eq!(
        store.matter().get(subscription).unwrap().unwrap(),
        b"home subscription"
    );
    let status = status(&store);
    let terminal = Context::committed(app, &status);
    let output = block_on(terminal.handle_line("status")).unwrap().unwrap();
    assert!(
        output.contains(&format!("root version {}", saved.root_version)),
        "{output}"
    );
    assert!(
        !block_on(terminal.handle_line("list"))
            .unwrap()
            .unwrap()
            .contains("Test Outlet")
    );
}

#[test]
fn failed_device_withdrawal_omits_the_retired_source_from_restored_reports() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let source = AppSource::new(&store, Some(outlet()));
    let app = App::open(source.handle.clone(), store.clone()).unwrap();
    let subscription = crate::matter::subscription_keys().start;
    store.matter().put(subscription, b"same home").unwrap();
    assert_eq!(app.matter_configuration().entries.len(), 1);
    source.remove(&outlet().key().id);
    let db = rusqlite::Connection::open(store.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_removal BEFORE UPDATE ON bridge_configuration
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let plan = app.plan().unwrap().unwrap();
    assert!(app.apply(app.prepare(plan)).is_err());
    assert!(app.matter_configuration().entries.is_empty());
    assert_eq!(
        store.matter().get(subscription).unwrap().unwrap(),
        b"same home"
    );
    db.execute_batch("DROP TRIGGER reject_removal").unwrap();
    let plan = app.plan().unwrap().unwrap();
    app.apply(app.prepare(plan)).unwrap();
    assert!(app.matter_configuration().entries.is_empty());
    assert_eq!(
        store.matter().get(subscription).unwrap().unwrap(),
        b"same home"
    );
}

#[test]
fn descriptions_from_the_previous_home_are_not_visible_for_the_same_did() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let source = AppSource::new(&store, Some(outlet()));
    let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
    source.scope(crate::configuration::Scope {
        home_id: "other-home".into(),
        ..app.current().configuration.scope.clone().unwrap()
    });
    source.describe(outlet());
    assert!(app.accessible_descriptions(&app.current()).is_empty());
    let status = status(&store);
    let terminal = Context::committed(app, &status);
    let list = block_on(terminal.handle_line("list")).unwrap().unwrap();
    assert!(!list.contains("Test Outlet"), "{list}");
    let detail = block_on(terminal.handle_line("status test-outlet"))
        .unwrap()
        .unwrap();
    assert!(detail.starts_with("Unknown device DID."), "{detail}");
}
