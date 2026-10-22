use super::{CatalogError, current_home, load_device, select_local_home};
use crate::xiaomi::capability::NativeDescription;
use crate::{
    configuration::Scope,
    device::DeviceKey,
    storage::XiaomiRecord,
    xiaomi::{
        cloud::{CloudClient, OwnedHome},
        discovery::DiscoverySnapshot,
    },
};
use crate::{
    storage::TokenSet,
    xiaomi::{
        cloud::{CloudErrorKind, group_id},
        discovery::Gateway,
        test_support::{MockResponse, mock_server},
    },
};
use futures_lite::future::block_on;
use serde_json::{Value, json};
use std::time::Duration;

fn credentials() -> XiaomiRecord {
    XiaomiRecord {
        uid: "account".into(),
        region: "cn".into(),
        oauth_client_uuid: "unused".into(),
        redirect_uri: "unused".into(),
        virtual_did: "client".into(),
        private_key_pem: "unused".into(),
        certificate_pem: "unused".into(),
        tokens: TokenSet {
            access_token: "processed-token".into(),
            refresh_token: "unused".into(),
            expires_at: 10,
            refresh_at: 9,
        },
    }
}

fn key() -> DeviceKey {
    DeviceKey::new("xiaomi", "target")
}

fn home(id: &str, dids: &[&str]) -> OwnedHome {
    OwnedHome {
        uid: "account".into(),
        home_id: id.into(),
        group_id: group_id("account", id),
        dids: dids.iter().map(|did| (*did).into()).collect(),
    }
}

fn snapshot() -> DiscoverySnapshot {
    DiscoverySnapshot {
        gateways: vec![Gateway {
            did: "hub".into(),
            group_id: group_id("account", "local"),
            addresses: vec!["192.0.2.2:8883".parse().unwrap()],
        }],
        revision: 1,
    }
}

fn instance() -> Value {
    serde_json::from_str(include_str!("../../../../tests/fixtures/miot/outlet.json")).unwrap()
}

fn detail(instance: &Value) -> MockResponse {
    MockResponse::json(200, &json!({"code":0,"result":{"list":[{"did":"target","model":"vendor.plug","spec_type":instance["type"],"name":"Cloud outlet"}]}}).to_string())
}

#[test]
fn local_home_selection_is_stable_and_independent_of_device_membership() {
    let mut discovery = snapshot();
    let mut gateway = discovery.gateways[0].clone();
    gateway.group_id = group_id("account", "a-local");
    discovery.gateways.push(gateway);
    let homes = [
        home("remote", &["remote"]),
        home("local", &["target"]),
        home("a-local", &[]),
    ];
    let selected = select_local_home(&discovery, &homes, "account").unwrap();
    assert_eq!(selected.home_id, "a-local");
    assert!(selected.dids.is_empty());
    let mut reversed = homes.clone();
    reversed.reverse();
    assert_eq!(
        select_local_home(&discovery, &reversed, "account").unwrap(),
        selected
    );
    assert!(matches!(
        select_local_home(&discovery, &homes, "another-account"),
        Err(CatalogError::NoLocalHome)
    ));
}

fn scope() -> Scope {
    Scope {
        uid: "account".into(),
        home_id: "local".into(),
    }
}

#[test]
fn current_home_requires_the_selected_account_and_home() {
    for homes in [
        vec![],
        vec![home("remote", &["target"])],
        vec![OwnedHome {
            uid: "other-account".into(),
            home_id: "local".into(),
            group_id: group_id("other-account", "local"),
            dids: vec!["target".into()],
        }],
    ] {
        assert!(matches!(
            current_home(&homes, &scope()),
            Err(CatalogError::NoLocalHome)
        ));
    }
    let empty = home("local", &[]);
    assert_eq!(
        current_home(&[home("remote", &["target"]), empty.clone()], &scope()).unwrap(),
        empty
    );
}

#[test]
fn fixed_home_loads_all_members_with_independent_single_and_multi_channel_bindings() {
    let single = instance();
    let multi: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/cuco-cp7pd.json"
    ))
    .unwrap();
    let selected = home("local", &["first", "second", "multi"]);
    let mut responses = Vec::new();
    for (did, model, instance) in [
        ("first", "vendor.plug", &single),
        ("second", "vendor.plug", &single),
        ("multi", "cuco.plug.cp7pd", &multi),
    ] {
        responses.push(MockResponse::json(200, &json!({"code":0,"result":{"list":[{"did":"remote-only"},{"did":did,"model":model,"spec_type":instance["type"]}]}}).to_string()));
        responses.push(MockResponse::json(200, &instance.to_string()));
    }
    let (base, requests) = mock_server(responses);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let selected = select_local_home(
        &snapshot(),
        &[home("remote", &["remote-only"]), selected],
        "account",
    )
    .unwrap();
    assert_eq!(selected.dids, ["first", "second", "multi"]);
    let records = block_on(async {
        let mut records = Vec::new();
        for did in &selected.dids {
            let (record, name) = load_device(&cloud, &credentials(), &selected, did)
                .await
                .unwrap();
            assert_eq!(name, None);
            records.push(record);
        }
        records
    });
    assert_eq!(records[0].model(), records[1].model());
    assert_ne!(records[0].device().key(), records[1].device().key());
    assert_eq!(records[0].device().functions()[0].description().id, "main");
    assert_eq!(
        records[2]
            .device()
            .functions()
            .iter()
            .map(|function| (
                function.id(),
                function.power().unwrap().siid,
                function.power().unwrap().piid
            ))
            .collect::<Vec<_>>(),
        [("outlet_1", 3, 1), ("outlet_2", 4, 1), ("usb", 9, 1)]
    );
    let captured = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(captured.len(), 6);
    for (request, did) in captured.iter().step_by(2).zip(&selected.dids) {
        assert_eq!(
            serde_json::from_str::<Value>(&request.body).unwrap()["dids"],
            json!([did])
        );
    }
    assert!(
        current_home(&[home("local", &[]), home("remote", &["first"])], &scope())
            .unwrap()
            .dids
            .is_empty()
    );
}

#[test]
fn scoped_load_rejects_foreign_accounts_and_members_without_network_requests() {
    let (base, requests) = mock_server(vec![MockResponse::json(500, "unused")]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let local = home("local", &["target"]);
    assert!(matches!(
        block_on(load_device(&cloud, &credentials(), &local, "remote-only")),
        Err(CatalogError::TargetMissing)
    ));
    let mut foreign = local.clone();
    foreign.uid = "foreign".into();
    foreign.group_id = group_id("foreign", "local");
    assert!(matches!(
        block_on(load_device(&cloud, &credentials(), &foreign, "target")),
        Err(CatalogError::InvalidScope)
    ));
    assert_eq!(requests.try_iter().count(), 0);
}

#[test]
fn fixed_home_load_reports_incompatible_capability() {
    let mut unsupported = instance();
    unsupported["services"][1]["properties"][0]["access"] = json!(["read", "notify"]);
    let (base, requests) = mock_server(vec![
        detail(&unsupported),
        MockResponse::json(200, &unsupported.to_string()),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert!(matches!(
        block_on(load_device(
            &cloud,
            &credentials(),
            &home("local", &["target"]),
            "target"
        )),
        Err(CatalogError::Capability(_))
    ));
    assert_eq!(requests.try_iter().count(), 2);
}

#[test]
fn loads_only_local_member_detail_and_spec_using_processed_credentials() {
    let instance = instance();
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &json!({"code":0,"result":{"list":[{"did":"other-home-device","name":"private"}],"has_more":true,"next_start_did":"next"}}).to_string()),
        detail(&instance),
        MockResponse::json(200, &instance.to_string()),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let homes = [
        home("remote", &["target", "remote-only"]),
        home("local", &["target"]),
        home("local-other", &["different"]),
    ];
    let (cached, name) = block_on(load_local(
        &snapshot(),
        &homes,
        &credentials(),
        &key(),
        &cloud,
    ))
    .unwrap();
    assert_eq!(
        select_local_home(&snapshot(), &homes, "account")
            .unwrap()
            .home_id,
        "local"
    );
    assert_eq!(cached.device().functions()[0].power().unwrap().siid, 2);
    assert_eq!(cached.device().functions()[0].power().unwrap().piid, 1);
    assert_eq!(name.as_deref(), Some("Cloud outlet"));
    let captured = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(captured.len(), 3);
    for request in &captured[..2] {
        assert_eq!(request.target, "/app/v2/home/device_list_page");
        assert!(request.headers.contains("Bearerprocessed-token"));
        assert_eq!(
            serde_json::from_str::<Value>(&request.body).unwrap()["dids"],
            json!(["target"])
        );
    }
    assert!(
        captured[2]
            .target
            .starts_with("/miot-spec-v2/instance?type=")
    );
    assert!(
        !captured[2]
            .headers
            .to_ascii_lowercase()
            .contains("authorization:")
    );
}

#[test]
fn no_local_home_remote_target_or_different_account_makes_no_request() {
    let (base, requests) = mock_server(vec![MockResponse::json(500, "unused")]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let credentials = credentials();
    let homes = [home("remote", &["target"]), home("local", &["different"])];
    let absent = DiscoverySnapshot {
        gateways: vec![],
        revision: 0,
    };
    assert!(matches!(
        block_on(load_local(&absent, &homes, &credentials, &key(), &cloud)),
        Err(CatalogError::NoLocalHome)
    ));
    assert!(matches!(
        block_on(load_local(
            &snapshot(),
            &homes,
            &credentials,
            &key(),
            &cloud
        )),
        Err(CatalogError::TargetMissing)
    ));
    let mut other_account = home("local", &["target"]);
    other_account.uid = "another-account".into();
    other_account.group_id = group_id("another-account", "local");
    assert!(matches!(
        block_on(load_local(
            &snapshot(),
            &[other_account],
            &credentials,
            &key(),
            &cloud
        )),
        Err(CatalogError::NoLocalHome)
    ));
    assert_eq!(requests.try_iter().count(), 0);
}

#[test]
fn missing_target_and_cloud_failure_are_returned_without_cache_fallback() {
    for (response, expected) in [
        (
            MockResponse::json(200, &json!({"code":0,"result":{"list":null}}).to_string()),
            None,
        ),
        (
            MockResponse::json(503, "{}"),
            Some(CloudErrorKind::HttpStatus(503)),
        ),
        (
            MockResponse::json(401, "{}"),
            Some(CloudErrorKind::Unauthorized),
        ),
    ] {
        let (base, requests) = mock_server(vec![response]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let result = block_on(load_local(
            &snapshot(),
            &[home("local", &["target"])],
            &credentials(),
            &key(),
            &cloud,
        ));
        match (result, expected) {
            (Err(CatalogError::TargetMissing), None) => {}
            (Err(CatalogError::Cloud(error)), Some(expected)) => {
                assert_eq!(error.kind(), &expected)
            }
            result => panic!("unexpected catalog result: {result:?}"),
        }
        assert_eq!(requests.try_iter().count(), 1);
    }
}

#[test]
fn unsupported_new_description_is_reported_as_capability_failure() {
    let mut unsupported = instance();
    unsupported["services"][1]["properties"][0]["access"] = json!(["read", "notify"]);
    let (base, requests) = mock_server(vec![
        detail(&unsupported),
        MockResponse::json(200, &unsupported.to_string()),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert!(matches!(
        block_on(load_local(
            &snapshot(),
            &[home("local", &["target"])],
            &credentials(),
            &key(),
            &cloud
        )),
        Err(CatalogError::Capability(_))
    ));
    assert_eq!(requests.try_iter().count(), 2);
}

#[test]
fn damaged_cloud_description_fails_before_requesting_an_instance() {
    for field in ["did", "model", "spec_type"] {
        let mut device =
            json!({"did":"target", "model":"vendor.plug", "spec_type":instance()["type"]});
        device[field] = json!(format!("{}\n", device[field].as_str().unwrap()));
        let (base, requests) = mock_server(vec![MockResponse::json(
            200,
            &json!({"code":0,"result":{"list":[device]}}).to_string(),
        )]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let result = block_on(load_local(
            &snapshot(),
            &[home("local", &["target"])],
            &credentials(),
            &key(),
            &cloud,
        ));
        assert!(
            matches!(result, Err(CatalogError::Cloud(error)) if *error.kind() == CloudErrorKind::Protocol)
        );
        assert_eq!(requests.try_iter().count(), 1);
    }
}

async fn load_local(
    snapshot: &DiscoverySnapshot,
    homes: &[OwnedHome],
    credentials: &XiaomiRecord,
    key: &DeviceKey,
    cloud: &CloudClient,
) -> Result<(NativeDescription, Option<String>), CatalogError> {
    let home = select_local_home(snapshot, homes, &credentials.uid)?;
    load_device(cloud, credentials, &home, &key.id).await
}

#[test]
fn light_catalog_uses_cloud_model_for_the_exact_ls2307_color_adapter() {
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/devcea-light-ls2307.json"
    ))
    .unwrap();
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &json!({"code":0,"result":{"list":[{"did":"target","model":"devcea.light.ls2307","spec_type":instance["type"],"name":"Bedroom light"}]}}).to_string()),
        MockResponse::json(200, &instance.to_string()),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let (cached, name) = block_on(load_device(
        &cloud,
        &credentials(),
        &home("local", &["target"]),
        "target",
    ))
    .unwrap();
    assert_eq!(name.as_deref(), Some("Bedroom light"));
    assert_eq!(cached.device().functions().len(), 1);
    let lighting = cached.device().functions()[0].lighting().unwrap();
    assert_eq!(lighting.color().unwrap().property().siid, 4);
    assert_eq!(lighting.mode().unwrap().property().piid, 2);
    assert_eq!(requests.try_iter().count(), 2);
}

#[test]
fn light_group_descriptors_load_for_confirmed_group_dids() {
    let mut instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"
    ))
    .unwrap();
    instance["type"] = json!("urn:miot-spec-v2:device:light:0000A001:mijia-group3:3:0000C802");
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &json!({"code":0,"result":{"list":[{"did":"group.42","model":"mijia.light.group3","spec_type":instance["type"],"name":"Grouped lights"}]}}).to_string()),
        MockResponse::json(200, &instance.to_string()),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let (cached, name) = block_on(load_device(
        &cloud,
        &credentials(),
        &home("local", &["group.42"]),
        "group.42",
    ))
    .unwrap();
    assert_eq!(cached.device().key().id, "group.42");
    assert_eq!(cached.model(), "mijia.light.group3");
    assert_eq!(name.as_deref(), Some("Grouped lights"));
    assert_eq!(requests.try_iter().count(), 2);
}
