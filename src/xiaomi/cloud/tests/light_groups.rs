use super::*;

const GROUP_MODEL: &str = "mijia.light.group3";
const GROUP_SPEC: &str = "urn:miot-spec-v2:device:light:0000A001:mijia-group3:3:0000C802";

fn ordinary(did: &str) -> Value {
    json!({
        "did": did,
        "model": "yeelink.light.light3",
        "spec_type": "urn:miot-spec-v2:device:light:0000A001:yeelink-light3:1:0000C802",
        "isOnline": true,
    })
}

fn group(did: &str, group_did: &str, is_physical: bool, members: &[&str]) -> Value {
    let mut member_ship = serde_json::Map::new();
    for member in members {
        member_ship.insert((*member).into(), json!("1"));
    }
    json!({
        "did": did,
        "model": GROUP_MODEL,
        "spec_type": GROUP_SPEC,
        "isOnline": true,
        "extra": {"member_list": {
            "group_did": group_did,
            "status": "1",
            "display_member_switch": 0,
            "member_ship": Value::Object(member_ship),
            "is_physical": is_physical,
        }},
    })
}

fn catalog_page(records: Vec<Value>, has_more: bool, cursor: Option<&str>) -> Value {
    let mut result = json!({"list": records, "has_more": has_more});
    if let Some(cursor) = cursor {
        result["next_start_did"] = json!(cursor);
    }
    result
}

fn read_catalog(responses: Vec<MockResponse>) -> Result<CloudDeviceCatalog, CloudError> {
    let (base, _) = mock_server(responses);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    future::block_on(cloud.get_device_catalog("access-secret"))
}

fn confirmed(catalog: &CloudDeviceCatalog, did: &str) -> CloudGroupMembership {
    let device = catalog
        .devices
        .iter()
        .find(|device| device.did == did)
        .unwrap_or_else(|| panic!("missing catalog record {did}"));
    match &device.group {
        CloudGroupStatus::Confirmed(membership) => membership.clone(),
        status => panic!("expected confirmed group for {did}, got {status:?}"),
    }
}

fn group_status(catalog: &CloudDeviceCatalog, did: &str) -> CloudGroupStatus {
    catalog
        .devices
        .iter()
        .find(|device| device.did == did)
        .unwrap_or_else(|| panic!("missing catalog record {did}"))
        .group
        .clone()
}

fn group_wire(did: &str, is_physical: bool, members: &[&str]) -> String {
    let member_ship = members
        .iter()
        .map(|member| format!(r#""{member}":"1""#))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"did":"{did}","model":"{GROUP_MODEL}","spec_type":"{GROUP_SPEC}","isOnline":true,"extra":{{"member_list":{{"group_did":"{did}","status":"1","display_member_switch":0,"member_ship":{{{member_ship}}},"is_physical":{is_physical}}}}}}}"#
    )
}

fn envelope(records: &[String], has_more: bool, cursor: Option<&str>) -> String {
    let list = records.join(",");
    let pagination = match cursor {
        Some(cursor) => format!(r#","has_more":{has_more},"next_start_did":"{cursor}""#),
        None => format!(r#","has_more":{has_more}"#),
    };
    format!(r#"{{"code":0,"result":{{"list":[{list}]{pagination}}}}}"#)
}

#[test]
fn device_catalog_reads_the_full_account_directory_without_dids_filter() {
    let responses = vec![
        response(catalog_page(
            vec![
                ordinary("plain-1"),
                group("group-1", "group-1", true, &["member-1", "member-2"]),
            ],
            true,
            Some("cursor-1"),
        )),
        response(catalog_page(
            vec![group(
                "group-3",
                "group-3",
                false,
                &["member-2", "member-3"],
            )],
            false,
            None,
        )),
    ];
    let (base, requests) = mock_server(responses);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let catalog = future::block_on(cloud.get_device_catalog("access-secret")).unwrap();

    assert_eq!(
        catalog
            .devices
            .iter()
            .map(|device| device.did.as_str())
            .collect::<Vec<_>>(),
        ["group-1", "group-3", "plain-1"]
    );
    let plain = catalog
        .devices
        .iter()
        .find(|device| device.did == "plain-1")
        .unwrap();
    assert_eq!(plain.model, "yeelink.light.light3");
    assert_eq!(
        plain.instance_type.as_deref(),
        Some("urn:miot-spec-v2:device:light:0000A001:yeelink-light3:1:0000C802")
    );
    assert_eq!(plain.name, None);
    assert!(plain.online);
    assert_eq!(plain.group, CloudGroupStatus::NotGroup);
    assert_eq!(
        confirmed(&catalog, "group-1"),
        CloudGroupMembership {
            group_did: "group-1".into(),
            is_physical: true,
            members: vec!["member-1".into(), "member-2".into()],
        }
    );
    assert_eq!(
        confirmed(&catalog, "group-3"),
        CloudGroupMembership {
            group_did: "group-3".into(),
            is_physical: false,
            members: vec!["member-2".into(), "member-3".into()],
        }
    );

    let captured = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(captured.len(), 2);
    for (index, request) in captured.iter().enumerate() {
        assert_eq!(request.target, "/app/v2/home/device_list_page");
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("authorization: beareraccess-secret")
        );
        let mut expected = json!({"limit":200,"get_split_device":true,"get_third_device":true});
        if index == 1 {
            expected["start_did"] = json!("cursor-1");
        }
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body, expected);
        assert!(body.get("dids").is_none());
    }
}

#[test]
fn device_catalog_parses_real_native_group_samples_with_physical_and_virtual_flags() {
    let records: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/xiaomi/native-light-groups.json"
    ))
    .unwrap();
    let catalog = read_catalog(vec![response(catalog_page(records, false, None))]).unwrap();

    assert_eq!(catalog.devices.len(), 8);
    assert!(catalog.devices.iter().all(|device| {
        device.model == GROUP_MODEL
            && device.instance_type.as_deref() == Some(GROUP_SPEC)
            && device.name.is_none()
            && device.online
    }));
    assert_eq!(
        confirmed(&catalog, "group-1"),
        CloudGroupMembership {
            group_did: "group-1".into(),
            is_physical: true,
            members: ["member-2", "member-3", "member-4", "member-5"]
                .map(String::from)
                .to_vec(),
        }
    );
    assert_eq!(
        confirmed(&catalog, "group-23"),
        CloudGroupMembership {
            group_did: "group-23".into(),
            is_physical: true,
            members: ["member-24", "member-25", "member-26", "member-27"]
                .map(String::from)
                .to_vec(),
        }
    );
    assert_eq!(
        confirmed(&catalog, "group-28"),
        CloudGroupMembership {
            group_did: "group-28".into(),
            is_physical: false,
            members: ["member-29", "member-30"].map(String::from).to_vec(),
        }
    );
    assert_eq!(
        confirmed(&catalog, "group-6").members,
        ["member-10", "member-7", "member-8", "member-9"]
            .map(String::from)
            .to_vec()
    );
}

#[test]
fn device_catalog_keeps_confirmed_membership_for_offline_groups() {
    let mut offline = group("group-1", "group-1", true, &["member-1", "member-2"]);
    offline["isOnline"] = json!(false);
    let catalog = read_catalog(vec![response(catalog_page(vec![offline], false, None))]).unwrap();

    let device = catalog
        .devices
        .iter()
        .find(|device| device.did == "group-1")
        .unwrap();
    assert!(!device.online);
    assert_eq!(
        confirmed(&catalog, "group-1"),
        CloudGroupMembership {
            group_did: "group-1".into(),
            is_physical: true,
            members: ["member-1", "member-2"].map(String::from).to_vec(),
        }
    );
}

#[test]
fn device_catalog_keeps_identity_and_membership_without_miot_description() {
    let mut speaker = ordinary("speaker");
    speaker["model"] = json!("miir.wifispeaker.ir01");
    speaker.as_object_mut().unwrap().remove("spec_type");
    let mut no_description_group = group("group-1", "group-1", true, &["member-1", "member-2"]);
    no_description_group
        .as_object_mut()
        .unwrap()
        .remove("spec_type");
    let mut null_description = ordinary("null-description");
    null_description["spec_type"] = json!(null);
    let mut empty_description = ordinary("empty-description");
    empty_description["spec_type"] = json!("");

    let catalog = read_catalog(vec![response(catalog_page(
        vec![
            speaker,
            no_description_group,
            null_description,
            empty_description,
            group("group-2", "group-2", false, &["member-2"]),
        ],
        false,
        None,
    ))])
    .unwrap();

    let speaker = catalog
        .devices
        .iter()
        .find(|device| device.did == "speaker")
        .unwrap();
    assert_eq!(speaker.model, "miir.wifispeaker.ir01");
    assert_eq!(speaker.instance_type, None);
    assert!(speaker.online);
    assert_eq!(speaker.group, CloudGroupStatus::NotGroup);
    for did in ["null-description", "empty-description"] {
        let device = catalog
            .devices
            .iter()
            .find(|device| device.did == did)
            .unwrap();
        assert_eq!(device.instance_type, None, "{did}");
        assert_eq!(device.group, CloudGroupStatus::NotGroup, "{did}");
    }

    assert_eq!(
        confirmed(&catalog, "group-1"),
        CloudGroupMembership {
            group_did: "group-1".into(),
            is_physical: true,
            members: ["member-1", "member-2"].map(String::from).to_vec(),
        }
    );
    assert_eq!(
        catalog
            .devices
            .iter()
            .find(|device| device.did == "group-1")
            .unwrap()
            .instance_type,
        None
    );
    assert_eq!(
        confirmed(&catalog, "group-2").members,
        ["member-2"].map(String::from).to_vec()
    );
}

#[test]
fn device_catalog_snapshot_is_invariant_to_record_and_member_order() {
    let ascending = envelope(
        &[
            group_wire("group-b", true, &["member-1", "member-2", "member-3"]),
            group_wire("group-a", false, &["member-9", "member-10"]),
        ],
        false,
        None,
    );
    let descending = envelope(
        &[
            group_wire("group-a", false, &["member-10", "member-9"]),
            group_wire("group-b", true, &["member-3", "member-2", "member-1"]),
        ],
        false,
        None,
    );
    let first = read_catalog(vec![MockResponse::json(200, &ascending)]).unwrap();
    let second = read_catalog(vec![MockResponse::json(200, &descending)]).unwrap();

    assert_eq!(first, second);
    assert_eq!(first.devices[0].did, "group-a");
    assert_eq!(first.devices[1].did, "group-b");
    assert_eq!(
        confirmed(&first, "group-a").members,
        ["member-10", "member-9"].map(String::from).to_vec()
    );
}

#[test]
fn device_catalog_keeps_ordinary_missing_and_confirmed_empty_membership_distinct() {
    let mut extra_only = ordinary("extra-only");
    extra_only["extra"] = json!({"other": true});
    let mut null_membership = ordinary("null-membership");
    null_membership["extra"] = json!({"member_list": null});
    let mut missing_membership = ordinary("missing-membership");
    missing_membership["extra"] = json!({
        "member_list": {"group_did": "missing-membership", "status": "1", "is_physical": true},
    });

    let catalog = read_catalog(vec![response(catalog_page(
        vec![
            ordinary("plain"),
            extra_only,
            null_membership,
            missing_membership,
            group("empty-group", "empty-group", true, &[]),
        ],
        false,
        None,
    ))])
    .unwrap();

    assert_eq!(group_status(&catalog, "plain"), CloudGroupStatus::NotGroup);
    assert_eq!(
        group_status(&catalog, "extra-only"),
        CloudGroupStatus::NotGroup
    );
    assert_eq!(
        group_status(&catalog, "null-membership"),
        CloudGroupStatus::Failed
    );
    assert_eq!(
        group_status(&catalog, "missing-membership"),
        CloudGroupStatus::Failed
    );
    assert_eq!(
        confirmed(&catalog, "empty-group"),
        CloudGroupMembership {
            group_did: "empty-group".into(),
            is_physical: true,
            members: Vec::new(),
        }
    );
}

#[test]
fn device_catalog_marks_malformed_group_membership_as_failed() {
    let valid = json!({
        "group_did": "group-x",
        "status": "1",
        "display_member_switch": 16,
        "member_ship": {"member-1": "1"},
        "is_physical": true,
        "unknown-field": ["kept", "ignored"],
    });
    let record = |member_list: Value| {
        json!({
            "did": "group-x",
            "model": GROUP_MODEL,
            "spec_type": GROUP_SPEC,
            "isOnline": true,
            "extra": {"member_list": member_list},
        })
    };
    let mut cases: Vec<(String, Value)> = Vec::new();
    for member_list in [
        json!(null),
        json!("member_list"),
        json!(["member-1"]),
        json!(["group-array", "1", {"member-1": "1"}, true]),
        json!(7),
    ] {
        cases.push((format!("member_list {member_list}"), record(member_list)));
    }
    let mutate = |remove: &[&str], set: &[(&str, Value)]| {
        let mut member_list = valid.clone();
        for key in remove {
            member_list.as_object_mut().unwrap().remove(*key);
        }
        for (key, value) in set {
            member_list[key] = value.clone();
        }
        member_list
    };
    for (label, member_list) in [
        ("missing group_did", mutate(&["group_did"], &[])),
        ("empty group_did", mutate(&[], &[("group_did", json!(""))])),
        (
            "control group_did",
            mutate(&[], &[("group_did", json!("group\nx"))]),
        ),
        (
            "numeric group_did",
            mutate(&[], &[("group_did", json!(17))]),
        ),
        ("missing status", mutate(&["status"], &[])),
        ("zero status", mutate(&[], &[("status", json!("0"))])),
        ("other status", mutate(&[], &[("status", json!("2"))])),
        ("empty status", mutate(&[], &[("status", json!(""))])),
        ("numeric status", mutate(&[], &[("status", json!(1))])),
        ("boolean status", mutate(&[], &[("status", json!(true))])),
        ("null status", mutate(&[], &[("status", json!(null))])),
        ("missing is_physical", mutate(&["is_physical"], &[])),
        (
            "numeric is_physical",
            mutate(&[], &[("is_physical", json!(1))]),
        ),
        (
            "string is_physical",
            mutate(&[], &[("is_physical", json!("true"))]),
        ),
        (
            "null is_physical",
            mutate(&[], &[("is_physical", json!(null))]),
        ),
        ("missing member_ship", mutate(&["member_ship"], &[])),
        (
            "null member_ship",
            mutate(&[], &[("member_ship", json!(null))]),
        ),
        (
            "string member_ship",
            mutate(&[], &[("member_ship", json!("{}"))]),
        ),
        (
            "array member_ship",
            mutate(&[], &[("member_ship", json!([]))]),
        ),
        (
            "numeric member_ship",
            mutate(&[], &[("member_ship", json!(7))]),
        ),
        (
            "zero member status",
            mutate(&[], &[("member_ship", json!({"member-1": "0"}))]),
        ),
        (
            "numeric member status",
            mutate(&[], &[("member_ship", json!({"member-1": 1}))]),
        ),
        (
            "null member status",
            mutate(&[], &[("member_ship", json!({"member-1": null}))]),
        ),
        (
            "empty member did",
            mutate(&[], &[("member_ship", json!({"": "1"}))]),
        ),
        (
            "control member did",
            mutate(&[], &[("member_ship", json!({"member\n1": "1"}))]),
        ),
        (
            "self member",
            mutate(&[], &[("member_ship", json!({"group-x": "1"}))]),
        ),
    ] {
        cases.push((label.into(), record(member_list)));
    }

    for (label, record) in cases {
        let catalog = read_catalog(vec![response(catalog_page(vec![record], false, None))])
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(
            group_status(&catalog, "group-x"),
            CloudGroupStatus::Failed,
            "{label}"
        );
    }

    let catalog = read_catalog(vec![response(catalog_page(
        vec![record(valid)],
        false,
        None,
    ))])
    .unwrap();
    assert!(matches!(
        group_status(&catalog, "group-x"),
        CloudGroupStatus::Confirmed(_)
    ));
}

#[test]
fn device_catalog_marks_duplicate_member_keys_and_non_object_extra_as_failed() {
    let duplicate_member = format!(
        r#"{{"code":0,"result":{{"list":[{{"did":"group-x","model":"{GROUP_MODEL}","spec_type":"{GROUP_SPEC}","isOnline":true,"extra":{{"member_list":{{"group_did":"group-x","status":"1","member_ship":{{"member-1":"1","member-1":"1"}},"is_physical":true}}}}}}],"has_more":false}}}}"#
    );
    let catalog = read_catalog(vec![MockResponse::json(200, &duplicate_member)]).unwrap();
    assert_eq!(group_status(&catalog, "group-x"), CloudGroupStatus::Failed);

    let duplicate_list = format!(
        r#"{{"code":0,"result":{{"list":[{{"did":"group-x","model":"{GROUP_MODEL}","spec_type":"{GROUP_SPEC}","isOnline":true,"extra":{{"member_list":{{"group_did":"group-x","status":"1","member_ship":{{"member-1":"1"}},"is_physical":true}},"member_list":{{"group_did":"group-x","status":"1","member_ship":{{"member-2":"1"}},"is_physical":true}}}}}}],"has_more":false}}}}"#
    );
    let catalog = read_catalog(vec![MockResponse::json(200, &duplicate_list)]).unwrap();
    assert_eq!(group_status(&catalog, "group-x"), CloudGroupStatus::Failed);

    for extra in [json!("extra"), json!(7), json!(["extra"])] {
        let mut device = ordinary("plain");
        device["extra"] = extra.clone();
        let catalog = read_catalog(vec![response(catalog_page(vec![device], false, None))])
            .unwrap_or_else(|error| panic!("extra {extra}: {error}"));
        assert_eq!(
            group_status(&catalog, "plain"),
            CloudGroupStatus::Failed,
            "extra {extra}"
        );
    }
}

#[test]
fn device_catalog_isolates_group_read_failures_per_record() {
    let mut malformed = group("group-bad", "group-bad", true, &["member-2"]);
    malformed["extra"]["member_list"]["status"] = json!("0");
    let catalog = read_catalog(vec![response(catalog_page(
        vec![
            group("group-good", "group-good", true, &["member-1"]),
            malformed,
            ordinary("plain"),
            {
                let mut named = group("group-good-2", "group-good-2", false, &["member-2"]);
                named["name"] = json!("  ");
                named
            },
        ],
        false,
        None,
    ))])
    .unwrap();

    assert_eq!(
        confirmed(&catalog, "group-good").members,
        ["member-1"].map(String::from).to_vec()
    );
    assert_eq!(
        group_status(&catalog, "group-bad"),
        CloudGroupStatus::Failed
    );
    assert_eq!(group_status(&catalog, "plain"), CloudGroupStatus::NotGroup);
    assert_eq!(
        confirmed(&catalog, "group-good-2").members,
        ["member-2"].map(String::from).to_vec()
    );
    assert_eq!(
        catalog
            .devices
            .iter()
            .find(|device| device.did == "group-good-2")
            .unwrap()
            .name,
        None
    );
}

#[test]
fn device_catalog_rejects_duplicate_records_and_invalidates_duplicate_group_identities() {
    let responses = vec![
        response(catalog_page(
            vec![ordinary("plain")],
            true,
            Some("cursor-1"),
        )),
        response(catalog_page(vec![ordinary("plain")], false, None)),
    ];
    let (base, requests) = mock_server(responses);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.get_device_catalog("token"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
    assert_eq!(requests.try_iter().count(), 2);

    let catalog = read_catalog(vec![response(catalog_page(
        vec![
            group("group-1", "group-1", true, &["member-1"]),
            group("group-2", "group-1", true, &["member-2"]),
            group("group-3", "group-3", true, &["member-3"]),
            ordinary("group-9"),
            group("group-8", "group-9", true, &["member-4"]),
        ],
        false,
        None,
    ))])
    .unwrap();
    assert_eq!(group_status(&catalog, "group-1"), CloudGroupStatus::Failed);
    assert_eq!(group_status(&catalog, "group-2"), CloudGroupStatus::Failed);
    assert!(matches!(
        group_status(&catalog, "group-3"),
        CloudGroupStatus::Confirmed(_)
    ));
    assert_eq!(
        group_status(&catalog, "group-9"),
        CloudGroupStatus::NotGroup
    );
    assert_eq!(group_status(&catalog, "group-8"), CloudGroupStatus::Failed);
}

#[test]
fn device_catalog_rejects_malformed_record_identity() {
    for record in [
        json!({"model": GROUP_MODEL, "spec_type": GROUP_SPEC, "isOnline": true}),
        json!({"did": "", "model": GROUP_MODEL, "spec_type": GROUP_SPEC, "isOnline": true}),
        json!({"did": "bad\n", "model": GROUP_MODEL, "spec_type": GROUP_SPEC, "isOnline": true}),
        json!({"did": "plain", "spec_type": GROUP_SPEC, "isOnline": true}),
        json!({"did": "plain", "model": "", "spec_type": GROUP_SPEC, "isOnline": true}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": "bad\n", "isOnline": true}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": GROUP_SPEC, "isOnline": true, "name": 17}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": GROUP_SPEC}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": GROUP_SPEC, "isOnline": 1}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": GROUP_SPEC, "isOnline": "true"}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": GROUP_SPEC, "isOnline": null}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": 17, "isOnline": true}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": [], "isOnline": true}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": {}, "isOnline": true}),
        json!({"did": "plain", "model": GROUP_MODEL, "spec_type": false, "isOnline": true}),
    ] {
        let label = format!("record {record}");
        let error =
            read_catalog(vec![response(catalog_page(vec![record], false, None))]).unwrap_err();
        assert_eq!(error.kind(), &CloudErrorKind::Protocol, "{label}");
    }
}

#[test]
fn device_catalog_invalidates_the_snapshot_when_any_page_or_cursor_fails() {
    for (label, responses, expected_requests, expected_kind) in [
        (
            "first page http error",
            vec![MockResponse::json(503, "{}")],
            1,
            CloudErrorKind::HttpStatus(503),
        ),
        (
            "first page business error",
            vec![MockResponse::json(200, r#"{"code":-6,"message":"shared"}"#)],
            1,
            CloudErrorKind::Business(-6),
        ),
        (
            "first page invalid json",
            vec![MockResponse::json(200, "{")],
            1,
            CloudErrorKind::Protocol,
        ),
        (
            "missing has_more cursor",
            vec![response(json!({"list": [], "has_more": true}))],
            1,
            CloudErrorKind::Protocol,
        ),
        (
            "empty cursor",
            vec![response(json!({
                "list": [], "has_more": true, "next_start_did": "",
            }))],
            1,
            CloudErrorKind::Protocol,
        ),
        (
            "control cursor",
            vec![response(json!({
                "list": [], "has_more": true, "next_start_did": "bad\n",
            }))],
            1,
            CloudErrorKind::Protocol,
        ),
        (
            "second page failure",
            vec![
                response(catalog_page(vec![ordinary("plain")], true, Some("one"))),
                MockResponse::json(503, "{}"),
            ],
            2,
            CloudErrorKind::HttpStatus(503),
        ),
        (
            "second page duplicate cursor",
            vec![
                response(catalog_page(vec![ordinary("plain")], true, Some("same"))),
                response(catalog_page(vec![ordinary("other")], true, Some("same"))),
            ],
            2,
            CloudErrorKind::Protocol,
        ),
    ] {
        let (base, requests) = mock_server(responses);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let error = future::block_on(cloud.get_device_catalog("token")).unwrap_err();
        assert_eq!(error.kind(), &expected_kind, "{label}");
        assert_eq!(requests.try_iter().count(), expected_requests, "{label}");
    }

    let catalog = read_catalog(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":{"list":null,"has_more":false}}"#,
    )])
    .unwrap();
    assert!(catalog.devices.is_empty());
}
