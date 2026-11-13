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
