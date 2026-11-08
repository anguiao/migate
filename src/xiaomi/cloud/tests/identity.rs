use super::*;

#[test]
fn ha_identity_probe_is_minimal() {
    let dids = (0..160)
        .map(|index| format!("did-{index}"))
        .collect::<Vec<_>>();
    let home = json!({"code":0,"result":{"homelist":[{"uid":123,"dids":dids,"roomlist":[{"dids":["did-0","room-only"]}]}],"share_home_list":[{"uid":"shared"}],"has_more":true}});
    let (base, requests) = mock_server(vec![MockResponse::json(200, &home.to_string())]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let page = future::block_on(client.get_home("access-secret")).unwrap();
    assert_eq!(page.uid.as_deref(), Some("123"));
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
    assert!(requests.try_recv().is_err());
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
