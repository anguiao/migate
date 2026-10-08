use super::*;

#[test]
fn owned_catalog_rejects_incomplete_initial_room_membership() {
    for home in [
        json!({"uid":"account","id":"home"}),
        json!({"uid":"account","id":"home","roomlist":null}),
        json!({"uid":"account","id":"home","roomlist":{}}),
    ] {
        let (base, _) = mock_server(vec![MockResponse::json(
            200,
            &json!({"code":0,"result":{"homelist":[home]}}).to_string(),
        )]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(cloud.get_owned_homes("token"))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
}

#[test]
fn owned_catalog_rejects_undeclared_home_on_any_member_page() {
    for page in [
        json!({"info":[{"id":"foreign","dids":["foreign-did"]}]}),
        json!({"info":[{"id":"home","dids":["local-did"]},{"id":"foreign"}]}),
    ] {
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &json!({"code":0,"result":{"homelist":[{"uid":"account","id":"home","roomlist":[]}],"has_more":true,"max_id":"next"}}).to_string()),
            MockResponse::json(200, &json!({"code":0,"result":page}).to_string()),
        ]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(cloud.get_owned_homes("token"))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
        assert_eq!(requests.try_iter().count(), 2);
    }
}

#[test]
fn owned_catalog_validates_identifiers_in_initial_and_paged_membership() {
    for (initial, page) in [
        (json!({"uid":"","id":"home","roomlist":[]}), None),
        (json!({"uid":"account\n","id":"home","roomlist":[]}), None),
        (json!({"uid":"account","id":"","roomlist":[]}), None),
        (json!({"uid":"account","id":"home\n","roomlist":[]}), None),
        (
            json!({"uid":"account","id":"home","dids":[""],"roomlist":[]}),
            None,
        ),
        (
            json!({"uid":"account","id":"home","roomlist":[{"dids":["bad\n"]}]}),
            None,
        ),
        (
            json!({"uid":"account","id":"home","roomlist":[]}),
            Some(json!({"id":""})),
        ),
        (
            json!({"uid":"account","id":"home","roomlist":[]}),
            Some(json!({"id":"home","dids":[""]})),
        ),
        (
            json!({"uid":"account","id":"home","roomlist":[]}),
            Some(json!({"id":"home","roomlist":[{"dids":["bad\n"]}]})),
        ),
    ] {
        let mut result = json!({"homelist":[initial]});
        let mut responses = Vec::new();
        if page.is_some() {
            result["has_more"] = json!(true);
            result["max_id"] = json!("next");
        }
        responses.push(MockResponse::json(
            200,
            &json!({"code":0,"result":result}).to_string(),
        ));
        if let Some(page) = page {
            responses.push(MockResponse::json(
                200,
                &json!({"code":0,"result":{"info":[page]}}).to_string(),
            ));
        }
        let (base, _) = mock_server(responses);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(cloud.get_owned_homes("token"))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
}

#[test]
fn owned_catalog_accepts_legal_empty_membership_and_terminal_page() {
    for initial in [
        json!({"uid":123,"id":456,"roomlist":[]}),
        json!({"uid":"account","id":"home","dids":null,"roomlist":[{}, {"dids":null}]}),
    ] {
        let (base, _) = mock_server(vec![MockResponse::json(
            200,
            &json!({"code":0,"result":{"homelist":[initial]}}).to_string(),
        )]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert!(
            future::block_on(cloud.get_owned_homes("token"))
                .unwrap()
                .homes[0]
                .dids
                .is_empty()
        );
    }
    for terminal in [
        json!({"info":[]}),
        json!({"info":[{"id":"home","dids":null}]}),
    ] {
        let (base, _) = mock_server(vec![
            MockResponse::json(200, &json!({"code":0,"result":{"homelist":[{"uid":"account","id":"home","roomlist":[]}],"has_more":true,"max_id":"next"}}).to_string()),
            MockResponse::json(200, &json!({"code":0,"result":terminal}).to_string()),
        ]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert!(
            future::block_on(cloud.get_owned_homes("token"))
                .unwrap()
                .homes[0]
                .dids
                .is_empty()
        );
    }
}

#[test]
fn owned_catalog_rejects_failed_or_truncated_member_page_without_partial_result() {
    for page in [
        MockResponse::json(503, "{}"),
        MockResponse::json(200, r#"{"code":0,"result":{"info":["#),
    ] {
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &json!({"code":0,"result":{"homelist":[{"uid":"account","id":"home","roomlist":[],"dids":["first"]}],"has_more":true,"max_id":"next"}}).to_string()),
            page,
        ]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert!(future::block_on(cloud.get_owned_homes("token")).is_err());
        assert_eq!(requests.try_iter().count(), 2);
    }
}

#[test]
fn owned_catalog_paginates_and_only_loads_the_matched_homes_device() {
    assert_eq!(group_id("account", "local"), "db84f388bb162bf2");
    let responses = [
        json!({"homelist":[
            {"uid":"account","id":"local","dids":null,"roomlist":[]},
            {"uid":"account","id":"remote","dids":["remote-did"],"roomlist":[],"name":"private name"}
        ],"has_more":true,"max_id":"cursor-1"}),
        json!({"info":[{"id":"local","dids":["first"],"roomlist":[{"dids":["target"]}]},{"id":"remote","dids":["remote-extra"]}],"has_more":true,"max_id":"cursor-2"}),
        json!({"info":[{"id":"local","dids":null,"roomlist":null}],"has_more":false}),
        json!({"list":[{"did":"irrelevant","name":"private name"}],"has_more":true,"next_start_did":"device-cursor"}),
        json!({"list":[{"did":"target","model":"vendor.plug","spec_type":"instance-type","name":"Target outlet"}],"has_more":false}),
    ];
    let (base, requests) = mock_server(
        responses
            .into_iter()
            .map(|result| MockResponse::json(200, &json!({"code":0,"result":result}).to_string()))
            .collect(),
    );
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let catalog = future::block_on(cloud.get_owned_homes("token")).unwrap();
    assert_eq!(catalog.uid.as_deref(), Some("account"));
    let local = catalog
        .homes
        .iter()
        .find(|home| home.group_id == group_id("account", "local"))
        .unwrap();
    assert_eq!(local.dids, ["first", "target"]);
    assert!(
        future::block_on(cloud.get_device("token", local, "remote-did"))
            .unwrap()
            .is_none()
    );
    let device = future::block_on(cloud.get_device("token", local, "target"))
        .unwrap()
        .unwrap();
    assert_eq!(device.did, "target");
    assert_eq!(device.model, "vendor.plug");
    assert_eq!(device.instance_type, "instance-type");
    assert_eq!(device.name.as_deref(), Some("Target outlet"));
    assert!(!format!("{catalog:?} {device:?}").contains("private name"));
    let captured = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(captured.len(), 5);
    let page: Value = serde_json::from_str(&captured[1].body).unwrap();
    assert_eq!(page["start_id"], "cursor-1");
    for request in &captured[3..] {
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["dids"], json!(["target"]));
    }
    let body: Value = serde_json::from_str(&captured[4].body).unwrap();
    assert_eq!(body["start_did"], "device-cursor");
}

#[test]
fn catalog_rejects_invalid_or_repeated_cursors_and_accepts_null_lists() {
    for result in [json!({"homelist":null}), json!({"homelist":[]})] {
        let (base, _) = mock_server(vec![MockResponse::json(
            200,
            &json!({"code":0,"result":result}).to_string(),
        )]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert!(
            future::block_on(cloud.get_owned_homes("token"))
                .unwrap()
                .homes
                .is_empty()
        );
    }
    for result in [
        json!({}),
        json!({"homelist":{}}),
        json!({"homelist":[{"uid":"account","id":"home","roomlist":[],"dids":7}]}),
        json!({"homelist":[{"uid":"account","id":"home","roomlist":[]}],"has_more":true}),
        json!({"homelist":[{"uid":"account","id":"home","roomlist":[]}],"has_more":"yes"}),
    ] {
        let (base, _) = mock_server(vec![MockResponse::json(
            200,
            &json!({"code":0,"result":result}).to_string(),
        )]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(cloud.get_owned_homes("token"))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
    let initial = json!({"code":0,"result":{"homelist":[{"uid":"account","id":"home","roomlist":[]}],"has_more":true,"max_id":"same"}});
    let repeated = json!({"code":0,"result":{"info":[{"id":"home","dids":["new"]}],"has_more":true,"max_id":"same"}});
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &initial.to_string()),
        MockResponse::json(200, &repeated.to_string()),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.get_owned_homes("token"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
    assert_eq!(requests.try_iter().count(), 2);

    let repeated = json!({"code":0,"result":{"info":[{"id":"home","dids":null}],"has_more":true,"max_id":"same"}});
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &initial.to_string()),
        MockResponse::json(200, &repeated.to_string()),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.get_owned_homes("token"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
    assert_eq!(requests.try_iter().count(), 2);
}

#[test]
fn device_pages_reject_missing_fields_and_repeated_cursors() {
    let home = OwnedHome {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        dids: vec!["target".into()],
    };
    for result in [
        json!({}),
        json!({"list":{}}),
        json!({"list":[{"did":"target","model":"model"}]}),
        json!({"list":[{"did":"target","model":"model","spec_type":"instance-type","name":17}]}),
        json!({"list":[{"did":"other"}],"has_more":true}),
    ] {
        let (base, _) = mock_server(vec![MockResponse::json(
            200,
            &json!({"code":0,"result":result}).to_string(),
        )]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(cloud.get_device("token", &home, "target"))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
    let responses = [
        json!({"list":[{"did":"other"}],"has_more":true,"next_start_did":"one"}),
        json!({"list":[{"did":"other"}],"has_more":true,"next_start_did":"one"}),
    ];
    let (base, requests) = mock_server(
        responses
            .into_iter()
            .map(|result| MockResponse::json(200, &json!({"code":0,"result":result}).to_string()))
            .collect(),
    );
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.get_device("token", &home, "target"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
    assert_eq!(requests.try_iter().count(), 2);
    let (base, _) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":{"list":null}}"#,
    )]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert!(
        future::block_on(cloud.get_device("token", &home, "target"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn owned_catalog_reads_past_empty_and_overlapping_pages_before_returning_membership() {
    let responses = [
        json!({"homelist":[{"uid":"account","id":"home","dids":["first"],"roomlist":[]}],"has_more":true,"max_id":"one"}),
        json!({"info":[{"id":"home","dids":["first"]}],"has_more":true,"max_id":"two"}),
        json!({"info":[],"has_more":true,"max_id":"three"}),
        json!({"info":[{"id":"home","roomlist":[{"dids":["last"]}]}]}),
    ];
    let (base, requests) = mock_server(
        responses
            .into_iter()
            .map(|result| MockResponse::json(200, &json!({"code":0,"result":result}).to_string()))
            .collect(),
    );
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let catalog = future::block_on(cloud.get_owned_homes("token")).unwrap();
    assert_eq!(catalog.homes[0].dids, ["first", "last"]);
    assert_eq!(requests.try_iter().count(), 4);
}

#[test]
fn device_lookup_reads_past_empty_and_overlapping_pages_without_broadening_dids() {
    let home = OwnedHome {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        dids: vec!["target".into()],
    };
    let responses = [
        json!({"list":[],"has_more":true,"next_start_did":"one"}),
        json!({"list":[{"did":"other"}],"has_more":true,"next_start_did":"two"}),
        json!({"list":[{"did":"other"}],"has_more":true,"next_start_did":"three"}),
        json!({"list":[{"did":"target","model":"vendor.plug","spec_type":"instance-type"}]}),
    ];
    let (base, requests) = mock_server(
        responses
            .into_iter()
            .map(|result| MockResponse::json(200, &json!({"code":0,"result":result}).to_string()))
            .collect(),
    );
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let device = future::block_on(cloud.get_device("token", &home, "target"))
        .unwrap()
        .unwrap();
    assert_eq!(device.did, "target");
    assert_eq!(device.name, None);
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(requests.len(), 4);
    for request in requests {
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["dids"], json!(["target"]));
    }
}

#[test]
fn device_lookup_leaves_missing_or_unusable_names_unset() {
    let home = OwnedHome {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        dids: vec!["target".into()],
    };
    for name in [Value::Null, json!(""), json!("  "), json!("Outlet\n")] {
        let result = json!({"list":[{"did":"target","model":"vendor.plug","spec_type":"instance-type","name":name}]});
        let (base, _) = mock_server(vec![MockResponse::json(
            200,
            &json!({"code":0,"result":result}).to_string(),
        )]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let device = future::block_on(cloud.get_device("token", &home, "target"))
            .unwrap()
            .unwrap();
        assert_eq!(device.name, None);
    }
}

#[test]
fn home_and_device_requests_are_minimal_and_bounded() {
    let dids = (0..160)
        .map(|index| format!("did-{index}"))
        .collect::<Vec<_>>();
    let home = json!({"code":0,"result":{"homelist":[{"uid":123,"dids":dids,"roomlist":[{"dids":["did-0","room-only"]}]}],"share_home_list":[{"uid":"shared"}],"has_more":true}});
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &home.to_string()),
        MockResponse::json(200, r#"{"code":0,"result":{"list":[],"has_more":true}}"#),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let page = future::block_on(client.get_home("access-secret")).unwrap();
    assert_eq!(page.uid.as_deref(), Some("123"));
    assert_eq!(page.dids.len(), 150);
    future::block_on(client.get_devices("access-secret", &page.dids)).unwrap();
    let home_request = requests.recv().unwrap();
    assert!(home_request.target.ends_with("/app/v2/homeroom/gethome"));
    assert!(
        home_request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: beareraccess-secret")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&home_request.body).unwrap(),
        json!({"limit":150,"fetch_share":false,"fetch_share_dev":false,"plat_form":0,"app_ver":9})
    );
    let device_request = requests.recv().unwrap();
    let body: Value = serde_json::from_str(&device_request.body).unwrap();
    assert_eq!(body["dids"].as_array().unwrap().len(), 150);
    assert_eq!(body["limit"], 200);
}

#[test]
fn floating_point_uid_is_a_protocol_error() {
    let (base, _) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":{"homelist":[{"uid":12.5}]}}"#,
    )]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let error = future::block_on(client.get_home("access")).unwrap_err();
    assert_eq!(error.kind(), &CloudErrorKind::Protocol);
}
