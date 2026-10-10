use super::*;
use crate::xiaomi::test_support::{dynamic_mock_server, mock_server};
use futures_lite::future;
use serde_json::{Value, json};
use std::time::Duration;

#[test]
fn home_device_info_skips_empty_members_without_http() {
    let (base, requests) = mock_server(vec![response(json!({}))]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert!(
        future::block_on(get_home_devices(
            &cloud,
            "token",
            &home(Vec::<String>::new())
        ))
        .unwrap()
        .is_empty()
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn home_device_info_paginates_with_scoped_requests_and_explicit_online_state() {
    let mut first = device("first", true);
    first["name"] = json!(17);
    let responses = [
        json!({"list":[first, {"did":"foreign","model":null,"isOnline":"private"}],"has_more":true,"next_start_did":"one"}),
        json!({"list":[],"has_more":true,"next_start_did":"two"}),
        json!({"list":[device("second", false)],"has_more":false}),
    ];
    let (base, requests) = mock_server(responses.into_iter().map(response).collect());
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let devices = future::block_on(get_home_devices(
        &cloud,
        "access-secret",
        &home(["first", "second"]),
    ))
    .unwrap();
    assert_eq!(
        devices,
        [
            CloudDeviceInfo {
                did: "first".into(),
                model: "vendor.plug".into(),
                instance_type: "instance-type".into(),
                online: true,
                group: None,
            },
            CloudDeviceInfo {
                did: "second".into(),
                model: "vendor.plug".into(),
                instance_type: "instance-type".into(),
                online: false,
                group: None,
            },
        ]
    );
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(requests.len(), 3);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request.target, "/app/v2/home/device_list_page");
        let mut expected = json!({"limit":200,"get_split_device":true,"get_third_device":true,"dids":["first","second"]});
        if index > 0 {
            expected["start_did"] = json!(if index == 1 { "one" } else { "two" });
        }
        assert_eq!(
            serde_json::from_str::<Value>(&request.body).unwrap(),
            expected
        );
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("authorization: beareraccess-secret")
        );
    }
}

#[test]
fn home_device_info_batches_members_without_broadening_any_page() {
    let members = (0..401)
        .map(|index| format!("did-{index}"))
        .collect::<Vec<_>>();
    let expected_members = members.clone();
    let mut request_index = 0;
    let (base, requests) = dynamic_mock_server(6, move |request| {
        let body: Value = serde_json::from_str(&request.body).unwrap();
        let batch = request_index / 2;
        let requested = &expected_members[batch * 150..((batch + 1) * 150).min(401)];
        assert_eq!(body["dids"], json!(requested));
        assert_eq!(body["limit"], 200);
        let is_first = request_index % 2 == 0;
        request_index += 1;
        if is_first {
            assert!(body.get("start_did").is_none());
            response(json!({"list":[],"has_more":true,"next_start_did":"same-per-batch"}))
        } else {
            assert_eq!(body["start_did"], "same-per-batch");
            response(
                json!({"list":requested.iter().map(|did| device(did, true)).collect::<Vec<_>>(),"has_more":false}),
            )
        }
    });
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let devices = future::block_on(get_home_devices(&cloud, "token", &home(members))).unwrap();
    assert_eq!(devices.len(), 401);
    assert_eq!(requests.try_iter().count(), 6);
}

#[test]
fn home_device_info_batches_member_requests_at_150() {
    let members = (0..151)
        .map(|index| format!("did-{index}"))
        .collect::<Vec<_>>();
    let (base, requests) = mock_server(vec![
        response(json!({"list":[],"has_more":false})),
        response(json!({"list":[],"has_more":false})),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    future::block_on(get_home_devices(&cloud, "token", &home(members))).unwrap();
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    for (request, expected_count) in requests.iter().zip([150, 1]) {
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["dids"].as_array().unwrap().len(), expected_count);
        assert_eq!(body["limit"], 200);
    }
}

#[test]
fn home_device_info_accepts_only_is_online_wire_field() {
    let record =
        json!({"did":"target","model":"vendor.plug","spec_type":"instance-type","online":true});
    let (base, _) = mock_server(vec![response(json!({"list":[record],"has_more":false}))]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(get_home_devices(&cloud, "token", &home(["target"])))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
}

#[test]
fn home_device_info_missing_members_do_not_become_offline_records_or_remove_membership() {
    let (base, _) = mock_server(vec![response(json!({
        "list":[device("present", false), {"did":"foreign"}],
    }))]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let home = home(["present", "missing"]);
    let devices = future::block_on(get_home_devices(&cloud, "token", &home)).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].did, "present");
    assert!(!devices[0].online);
    assert_eq!(home.dids, ["present", "missing"]);
}

#[test]
fn home_device_info_requires_valid_target_fields_and_boolean_online() {
    let valid = device("target", true);
    let mut invalid = Vec::new();
    for field in ["did", "model", "spec_type", "isOnline"] {
        let mut record = valid.clone();
        record.as_object_mut().unwrap().remove(field);
        invalid.push(record);
    }
    for (field, values) in [
        (
            "did",
            vec![json!(""), json!("bad\n"), json!(17), Value::Null],
        ),
        (
            "model",
            vec![json!(""), json!("bad\n"), json!(17), Value::Null],
        ),
        (
            "spec_type",
            vec![json!(""), json!("bad\n"), json!(17), Value::Null],
        ),
        (
            "isOnline",
            vec![json!(0), json!(1), json!("true"), Value::Null],
        ),
    ] {
        for value in values {
            let mut record = valid.clone();
            record[field] = value;
            invalid.push(record);
        }
    }
    for record in invalid {
        let (base, _) = mock_server(vec![response(json!({"list":[record],"has_more":false}))]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(get_home_devices(&cloud, "token", &home(["target"])))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol,
            "{record}"
        );
    }
}

#[test]
fn home_device_info_rejects_incomplete_pages_and_repeated_target_dids_or_cursors() {
    let target = device("target", true);
    let invalid_pages = [
        json!({}),
        json!({"list":null,"has_more":false}),
        json!({"list":{},"has_more":false}),
        json!({"list":[target.clone()],"has_more":"true"}),
        json!({"list":[target.clone()],"has_more":true}),
        json!({"list":[target.clone()],"has_more":true,"next_start_did":""}),
        json!({"list":[target.clone()],"has_more":true,"next_start_did":"bad\n"}),
        json!({"list":[target.clone(),target.clone()],"has_more":false}),
    ];
    for page in invalid_pages {
        let (base, _) = mock_server(vec![response(page.clone())]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(get_home_devices(&cloud, "token", &home(["target"])))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol,
            "{page}"
        );
    }
    for second in [
        json!({"list":[target.clone()],"has_more":false}),
        json!({"list":[],"has_more":true,"next_start_did":"same"}),
    ] {
        let (base, requests) = mock_server(vec![
            response(json!({"list":[target.clone()],"has_more":true,"next_start_did":"same"})),
            response(second),
        ]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(get_home_devices(&cloud, "token", &home(["target"])))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
        assert_eq!(requests.try_iter().count(), 2);
    }
}

#[test]
fn home_device_info_does_not_return_earlier_pages_or_batches_when_later_work_fails() {
    let dids = (0..201)
        .map(|index| format!("did-{index}"))
        .collect::<Vec<_>>();
    let (base, requests) = mock_server(vec![
        response(json!({"list":[device("did-0", true)],"has_more":false})),
        response(
            json!({"list":[{"did":"did-200","model":"vendor.plug","spec_type":"instance-type"}],"has_more":false}),
        ),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(get_home_devices(&cloud, "token", &home(dids)))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
    assert_eq!(requests.try_iter().count(), 2);
}

#[test]
fn home_device_info_rejects_invalid_requested_members_before_http() {
    let (base, requests) = mock_server(vec![response(json!({}))]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    for dids in [vec![""], vec!["bad\n"], vec!["target", "target"]] {
        assert_eq!(
            future::block_on(get_home_devices(&cloud, "token", &home(dids)))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::InvalidInput
        );
    }
    assert!(requests.try_recv().is_err());
}

#[test]
fn home_device_info_limits_requests_and_strict_validation_to_selected_members() {
    let (base, requests) = mock_server(vec![response(json!({
        "list":[device("target", true), {"did":"router","model":null,"isOnline":"invalid"}],
        "has_more":false,
    }))]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let selected_home = home(["target", "router"]);
    let devices =
        future::block_on(cloud.get_home_devices("token", &selected_home, &["target".into()]))
            .unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].did, "target");
    let request = requests.recv().unwrap();
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["dids"], json!(["target"]));
}

#[test]
fn home_device_info_rejects_requested_devices_outside_the_selected_home() {
    let (base, requests) = mock_server(vec![response(json!({}))]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.get_home_devices("token", &home(["target"]), &["foreign".into()]))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::InvalidInput
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn home_device_info_skips_http_for_no_requested_members() {
    let (base, requests) = mock_server(vec![response(json!({}))]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert!(
        future::block_on(cloud.get_home_devices("token", &home(["unrequested"]), &[]))
            .unwrap()
            .is_empty()
    );
    assert!(requests.try_recv().is_err());
}

fn group_record(did: &str, name: &str, physical: bool, members: Value) -> Value {
    json!({
        "did": did,
        "model": "mijia.light.group3",
        "spec_type": "instance-type",
        "isOnline": true,
        "name": name,
        "extra": {
            "member_list": {
                "group_did": did,
                "status": "1",
                "display_member_switch": 0,
                "member_ship": members,
                "is_physical": physical,
            }
        }
    })
}

#[test]
fn home_device_info_parses_physical_virtual_and_empty_group_metadata() {
    let records = [
        group_record(
            "group.physical",
            "Physical group",
            true,
            json!({"member-b":"1","member-a":"1"}),
        ),
        group_record("group.virtual", "Virtual group", false, json!({"only":"1"})),
        group_record("group.empty", "Empty group", true, json!({})),
        device("plain", true),
    ];
    let (base, _) = mock_server(vec![response(json!({"list":records}))]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let devices = future::block_on(get_home_devices(
        &cloud,
        "token",
        &home(["group.physical", "group.virtual", "group.empty", "plain"]),
    ))
    .unwrap();
    assert_eq!(
        devices
            .iter()
            .map(|device| device.group.clone())
            .collect::<Vec<_>>(),
        [
            Some(CloudGroup::Confirmed {
                did: "group.physical".into(),
                name: Some("Physical group".into()),
                physical: true,
                members: vec!["member-a".into(), "member-b".into()],
            }),
            Some(CloudGroup::Confirmed {
                did: "group.virtual".into(),
                name: Some("Virtual group".into()),
                physical: false,
                members: vec!["only".into()],
            }),
            Some(CloudGroup::Confirmed {
                did: "group.empty".into(),
                name: Some("Empty group".into()),
                physical: true,
                members: Vec::new(),
            }),
            None,
        ]
    );
}

#[test]
fn home_device_info_marks_known_groups_without_confirmed_relations() {
    let mut missing_members = group_record("group.missing", "Grouped", true, json!({}));
    missing_members["extra"]
        .as_object_mut()
        .unwrap()
        .remove("member_list");
    let mut null_members = group_record("group.null", "Grouped", true, json!({}));
    null_members["extra"]["member_list"] = Value::Null;
    let records = [
        group_record("group.empty", "Empty group", true, json!({})),
        group_record("group.confirmed", "Grouped", true, json!({"member":"1"})),
        json!({
            "did":"group.nometa",
            "model":"xiaomi.light.ceil04",
            "spec_type":"instance-type",
            "isOnline":true,
        }),
        json!({
            "did":"42",
            "model":"mijia.light.group3",
            "spec_type":"instance-type",
            "isOnline":true,
        }),
        missing_members,
        null_members,
    ];
    let (base, _) = mock_server(vec![response(json!({"list":records}))]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let devices = future::block_on(get_home_devices(
        &cloud,
        "token",
        &home([
            "group.empty",
            "group.confirmed",
            "group.nometa",
            "42",
            "group.missing",
            "group.null",
        ]),
    ))
    .unwrap();
    // A confirmed empty group stays distinct from a known group without metadata.
    assert_eq!(
        devices[0].group,
        Some(CloudGroup::Confirmed {
            did: "group.empty".into(),
            name: Some("Empty group".into()),
            physical: true,
            members: Vec::new(),
        })
    );
    assert_eq!(
        devices[1].group,
        Some(CloudGroup::Confirmed {
            did: "group.confirmed".into(),
            name: Some("Grouped".into()),
            physical: true,
            members: vec!["member".into()],
        })
    );
    for device in &devices[2..] {
        assert_eq!(
            device.group,
            Some(CloudGroup::Unconfirmed),
            "{}",
            device.did
        );
    }
}

#[test]
fn home_device_info_rejects_group_relations_that_contradict_their_identity() {
    for (did, model, group_did) in [
        ("group.42", "xiaomi.light.ceil04", "group.other"),
        ("42", "mijia.light.group3", "group.other"),
        ("42", "vendor.plug", "group.other"),
    ] {
        let mut record = group_record(did, "Grouped", true, json!({"member":"1"}));
        record["model"] = json!(model);
        record["extra"]["member_list"]["group_did"] = json!(group_did);
        let (base, _) = mock_server(vec![response(json!({"list":[record]}))]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(get_home_devices(&cloud, "token", &home([did])))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
}

#[test]
fn home_device_info_rejects_unconfirmed_group_and_member_states() {
    let invalid = [
        ("status", json!("0")),
        ("status", json!("2")),
        ("status", json!("")),
        ("status", json!(1)),
        ("status", Value::Null),
        ("is_physical", json!("true")),
        ("is_physical", Value::Null),
    ];
    for (field, value) in invalid {
        let mut record = group_record("group.bad", "Bad group", true, json!({"member":"1"}));
        record["extra"]["member_list"][field] = value;
        let (base, _) = mock_server(vec![response(json!({"list":[record]}))]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(get_home_devices(&cloud, "token", &home(["group.bad"])))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
    for (member, status) in [
        ("member", json!("0")),
        ("member", json!("2")),
        ("member", json!("")),
        ("member", json!(1)),
        ("member", Value::Null),
    ] {
        let mut record = group_record("group.bad", "Bad group", true, json!({}));
        record["extra"]["member_list"]["member_ship"][member] = status;
        let (base, _) = mock_server(vec![response(json!({"list":[record]}))]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(get_home_devices(&cloud, "token", &home(["group.bad"])))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
}

#[test]
fn home_device_info_rejects_malformed_group_member_lists() {
    let mut missing_members = group_record("group.bad", "Bad group", true, json!({}));
    missing_members["extra"]["member_list"]
        .as_object_mut()
        .unwrap()
        .remove("member_ship");
    let mut missing_physical = group_record("group.bad", "Bad group", true, json!({}));
    missing_physical["extra"]["member_list"]
        .as_object_mut()
        .unwrap()
        .remove("is_physical");
    let mut bad_did = group_record("group.bad", "Bad group", true, json!({}));
    bad_did["extra"]["member_list"]["group_did"] = json!("");
    let mut bad_extra = group_record("group.bad", "Bad group", true, json!({}));
    bad_extra["extra"] = json!("unexpected");
    let records = [
        missing_members,
        missing_physical,
        bad_did,
        bad_extra,
        group_record("group.bad", "Bad group", true, Value::Null),
        group_record("group.bad", "Bad group", true, json!([])),
        group_record("group.bad", "Bad group", true, json!({"bad\n":"1"})),
        group_record("group.bad", "Bad group", true, json!({"":"1"})),
    ];
    for record in records {
        let (base, _) = mock_server(vec![response(json!({"list":[record]}))]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(get_home_devices(&cloud, "token", &home(["group.bad"])))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
}

#[test]
fn home_device_info_ignores_malformed_member_lists_outside_selected_members() {
    let mut foreign = group_record("foreign", "Foreign group", true, json!({}));
    foreign["extra"]["member_list"]["status"] = json!("0");
    let (base, _) = mock_server(vec![response(
        json!({"list":[foreign, device("target", true)]}),
    )]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let devices = future::block_on(get_home_devices(&cloud, "token", &home(["target"]))).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].did, "target");
    assert_eq!(devices[0].group, None);
}

#[test]
fn home_device_info_tolerates_unusable_group_names() {
    for name in [
        Value::Null,
        json!(""),
        json!("  "),
        json!("Group\n"),
        json!(17),
    ] {
        let mut record = group_record("group.named", "Named group", true, json!({}));
        record["name"] = name;
        let (base, _) = mock_server(vec![response(json!({"list":[record]}))]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let devices =
            future::block_on(get_home_devices(&cloud, "token", &home(["group.named"]))).unwrap();
        assert!(matches!(
            &devices[0].group,
            Some(CloudGroup::Confirmed { name: None, .. })
        ));
    }
}
