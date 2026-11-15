use super::*;

#[test]
fn all_home_member_and_device_pages_are_scoped_and_preserve_group_relations() {
    let (base, requests) = mock_server(vec![
        response(
            json!({"homelist":[{"uid":123,"id":9,"dids":["one"],"roomlist":[{"dids":["two"]}]}],"has_more":true,"max_id":"next"}),
        ),
        response(
            json!({"info":[{"id":9,"dids":["three"],"roomlist":[{"dids":["one"]}]}],"has_more":false}),
        ),
        response(
            json!({"list":[{"did":"one","model":"vendor.light","spec_type":"light-type","isOnline":true,"pid":0,"localip":"192.0.2.5","token":"00112233445566778899aabbccddeeff"}, {"did":"outsider","model":"outside","spec_type":"outside-type","isOnline":true}],"has_more":true,"next_start_did":"cursor"}),
        ),
        response(
            json!({"list":[{"did":"two","model":"vendor.light","spec_type":"light-type","isOnline":false,"pid":1}],"has_more":false}),
        ),
        response(
            json!({"list":[{"did":"group","model":"vendor.group","spec_type":"group-type","isOnline":true,"extra":{"member_list":{"group_did":"group","status":"1","member_ship":{"two":"1","one":"1"},"is_physical":true}}}],"has_more":true,"next_start_did":"group"}),
        ),
        response(
            json!({"list":[{"did":"one","model":"vendor.light","spec_type":"light-type","isOnline":true}],"has_more":false}),
        ),
    ]);
    let client = client(&base, Duration::from_secs(1));
    let session = session();
    let homes = block_on(client.get_owned_homes(&session)).unwrap();
    assert_eq!(homes.uid.as_deref(), Some("123"));
    assert_eq!(homes.homes[0].dids, ["one", "two", "three"]);
    let home = &homes.homes[0];
    let devices = block_on(client.get_home_devices(&session, home, &home.dids)).unwrap();
    assert_eq!(devices.len(), 2);
    assert!(devices[0].online);
    assert!(matches!(devices[0].lan, LanCredentials::Candidate(_)));
    assert!(!devices[1].online);
    let catalog = block_on(client.get_device_catalog(&session)).unwrap();
    assert_eq!(catalog.devices.len(), 2);
    assert!(
        matches!(&catalog.devices[0].group, CloudGroupStatus::Confirmed(group) if group.members == ["one", "two"])
    );
    let captured: Vec<_> = (0..6).map(|_| requests.recv().unwrap()).collect();
    for request in &captured {
        assert_signed(request);
    }
    assert_eq!(captured[0].target, "/app/v2/homeroom/gethome");
    assert_eq!(captured[1].target, "/app/v2/homeroom/get_dev_room_page");
    assert_eq!(data(&captured[1])["start_id"], "next");
    assert_eq!(data(&captured[2])["dids"], json!(["one", "two", "three"]));
    assert_eq!(data(&captured[3])["start_did"], "cursor");
    assert!(data(&captured[4]).get("dids").is_none());
    assert_eq!(data(&captured[5])["start_did"], "group");
    assert!(requests.try_recv().is_err());
}

#[test]
fn directory_rejects_bad_or_failed_later_page_instead_of_returning_partial() {
    for (second, expected) in [
        (
            MockResponse::json(429, "{}"),
            CloudErrorKind::HttpStatus(429),
        ),
        (
            response(json!({"info":[],"has_more":true,"max_id":"cursor"})),
            CloudErrorKind::Protocol,
        ),
    ] {
        let (base, requests) = mock_server(vec![
            response(
                json!({"homelist":[{"uid":123,"id":9,"dids":["one"],"roomlist":[]}],"has_more":true,"max_id":"cursor"}),
            ),
            second,
        ]);
        let client = client(&base, Duration::from_secs(1));
        let error = block_on(client.get_owned_homes(&session())).unwrap_err();
        assert_eq!(error.kind(), &expected);
        assert_eq!(requests.try_iter().count(), 2);
    }
}

#[test]
fn home_lookup_never_requests_a_device_outside_the_selected_home() {
    let (base, requests) = mock_server(vec![response(json!({"list":[]}))]);
    let client = client(&base, Duration::from_secs(1));
    let home = crate::xiaomi::mijia::OwnedHome {
        uid: "123".into(),
        home_id: "9".into(),
        group_id: "group".into(),
        dids: vec!["one".into()],
    };
    assert!(
        block_on(client.get_device(&session(), &home, "outsider"))
            .unwrap()
            .is_none()
    );
    let error =
        block_on(client.get_home_devices(&session(), &home, &["outsider".into()])).unwrap_err();
    assert_eq!(error.kind(), &CloudErrorKind::InvalidInput);
    assert!(requests.try_recv().is_err());
}
