use crate::{
    app::App,
    device::{Availability, DeviceRequest, RelayMode},
    matter::tests::support::PublishedRelayModel,
    storage::{Store, tests::fixtures},
    xiaomi::{adapter::tests::app_source::AppSource, capability::tests::source::SourceFixture},
};
use futures_lite::future::{block_on, poll_once};
use std::rc::Rc;

#[test]
fn cold_restored_wired_relay_can_be_controlled_before_new_mode_and_power_reads() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let fixture = SourceFixture::new(
                    "panel",
                    "zimi.switch.dhkg01",
                    serde_json::from_str(include_str!(
                        "../../../tests/fixtures/miot/zimi-dhkg01.json"
                    ))
                    .unwrap(),
                );
                let mut configuration = fixtures::planned(&store, vec![fixture]);
                configuration.relay_channels[0].mode = Some(RelayMode::Wired);
                configuration.relay_channels[0].label = "Saved relay".into();
                fixtures::save(&store, &configuration);
                let endpoint = configuration.relay_channels[0].endpoint;
                let unique_id = configuration.relay_channels[0].unique_id.clone();
                drop(store);

                let store = Store::open(directory.path()).unwrap();
                let mut source = AppSource::new(&store, None);
                source.confirm_all();
                let physical = source.physical.as_ref().unwrap();
                physical.set_availability(Availability::Available);
                assert_eq!(
                    physical
                        .function("relay:2")
                        .unwrap()
                        .snapshot()
                        .relay_mode(),
                    None
                );
                let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
                let model = PublishedRelayModel::new(&app, &store);
                model.check_topology(&[endpoint]);
                assert_eq!(model.label(endpoint).await, "Saved relay");
                assert_eq!(model.unique_id(endpoint).await, unique_id);
                assert!(model.power(endpoint).await.is_err());
                let mut invoke = Box::pin(model.invoke(endpoint, 1));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let DeviceRequest::Power(request) =
                    source.receivers[0].next_operation().await.unwrap()
                else {
                    panic!("expected power request")
                };
                assert_eq!(request.relay_home(), Some(("account", "home")));
                assert!(request.target);
                request.accept().unwrap();
                invoke.await.unwrap();

                source.forbid("panel");
                assert!(model.invoke(endpoint, 0).await.is_err());
                assert!(
                    poll_once(source.receivers[0].next_operation())
                        .await
                        .is_none()
                );
                source.allow("panel");
                source.scope(crate::configuration::Scope {
                    uid: "account".into(),
                    home_id: "other".into(),
                });
                assert!(model.invoke(endpoint, 0).await.is_err());
                assert!(
                    poll_once(source.receivers[0].next_operation())
                        .await
                        .is_none()
                );
                source.scope(fixtures::scope());
                source.remove("panel");
                assert!(model.invoke(endpoint, 0).await.is_err());
            })
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn terminal_mapping_checks_authority_before_duplicates_and_failed_writes_do_not_retry() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let fixture = SourceFixture::new(
            "panel",
            "zimi.switch.dhkg01",
            serde_json::from_str(include_str!(
                "../../../tests/fixtures/miot/zimi-dhkg01.json"
            ))
            .unwrap(),
        );
        let mut configuration = fixtures::planned(&store, vec![fixture]);
        configuration.relay_channels[0].mode = Some(RelayMode::Wireless);
        fixtures::save(&store, &configuration);
        let source = AppSource::new(&store, None);
        source.confirm_all();
        let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
        let status = super::status(&store);
        let terminal = crate::terminal::bridge::Context::committed(app.clone(), &status);
        let query = |command: &'static str| async {
            terminal.handle_line(command, 0).await.unwrap().unwrap()
        };
        let database = rusqlite::Connection::open(store.path()).unwrap();
        database.execute_batch("CREATE TRIGGER fail_mapping BEFORE UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        assert!(
            query("map-type panel relay:2 switch")
                .await
                .contains("already mapped")
        );
        assert!(
            query("map-type panel relay:2 light")
                .await
                .contains("Mapping failed")
        );
        assert_eq!(app.current().configuration, configuration);
        assert_eq!(store.devices().load_configuration().unwrap(), configuration);
        assert!(!app.pending());
        assert!(app.plan().unwrap().is_none());
        source.forbid("panel");
        assert!(
            query("map-type panel relay:2 switch")
                .await
                .contains("no current access")
        );
        assert!(
            query("status panel")
                .await
                .contains("hidden (current access unavailable)")
        );
        source.allow("panel");
        assert!(
            query("map-type missing relay:2 switch")
                .await
                .contains("current home")
        );
        assert!(
            query("map-type panel relay:99 switch")
                .await
                .contains("Function does not exist")
        );
        assert!(
            query("map-type panel relay:2 dimmer")
                .await
                .contains("Unknown mapping type")
        );
        database.execute_batch("DROP TRIGGER fail_mapping").unwrap();
        assert!(
            query("map-type panel relay:2 light")
                .await
                .contains("mapped to light")
        );
        let after = app.current().configuration.clone();
        assert_eq!(
            after.relay_channels[0].device_type,
            crate::configuration::RelayType::Light
        );
        assert_eq!(
            after.relay_channels[0].configuration_version,
            configuration.relay_channels[0].configuration_version
        );
        assert_eq!(after.root_version, configuration.root_version);
        assert!(!app.pending());
        assert!(app.plan().unwrap().is_none());
        let list = query("list").await;
        assert!(
            list.contains("mode unknown (saved wireless); mapping light"),
            "{list}"
        );
        assert!(
            list.contains("hidden (saved wireless mode; awaiting current reading)"),
            "{list}"
        );
        let reopened = Store::open(directory.path()).unwrap();
        assert_eq!(reopened.devices().load_configuration().unwrap(), after);
        assert!(
            query("map-type panel relay:2 light")
                .await
                .contains("already mapped")
        );
        source
            .physical
            .as_ref()
            .unwrap()
            .function("relay:2")
            .unwrap()
            .observe_relay_mode(RelayMode::Wired);
        let plan = app.plan().unwrap().unwrap();
        assert!(plan.rebuild);
        app.apply(app.prepare(plan)).unwrap();
        let model = PublishedRelayModel::new(&app, &store);
        model.check_relay_type(after.relay_channels[0].endpoint, true);
        assert_eq!(
            model.unique_id(after.relay_channels[0].endpoint).await,
            after.relay_channels[0].unique_id
        );
    });
}
