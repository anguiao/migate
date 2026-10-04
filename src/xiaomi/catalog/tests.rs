use super::{CatalogError, load_scoped};
use crate::{
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
    serde_json::from_str(include_str!("../../../tests/fixtures/miot/outlet.json")).unwrap()
}

fn detail(instance: &Value) -> MockResponse {
    MockResponse::json(200, &json!({"code":0,"result":{"list":[{"did":"target","model":"vendor.plug","spec_type":instance["type"],"name":"not retained"}]}}).to_string())
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
    let cached = block_on(load_scoped(
        &snapshot(),
        &homes,
        &credentials(),
        &key(),
        &cloud,
    ))
    .unwrap();
    assert_eq!(cached.home_id, "local");
    assert_eq!(cached.group_id, group_id("account", "local"));
    assert_eq!(cached.outlet.main_power.siid, 2);
    assert_eq!(cached.outlet.main_power.piid, 1);
    assert!(!format!("{cached:?}").contains("not retained"));
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
        block_on(load_scoped(&absent, &homes, &credentials, &key(), &cloud)),
        Err(CatalogError::NoLocalHome)
    ));
    assert!(matches!(
        block_on(load_scoped(
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
    assert!(matches!(
        block_on(load_scoped(
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
        let result = block_on(load_scoped(
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
        block_on(load_scoped(
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
        let result = block_on(load_scoped(
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
