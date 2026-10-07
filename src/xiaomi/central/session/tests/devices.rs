use super::*;

#[test]
fn list_metadata_requires_explicit_access_flags() {
    let kind = RequestKind::List {
        did: "target".into(),
    };
    for info in [
        json!({"model":"cuco.plug.v3","urn":"outlet"}),
        json!({"model":"cuco.plug.v3","urn":"outlet","online":false,"specV2Access":true,"pushAvailable":0}),
    ] {
        assert!(matches!(
            parse_response(&kind, &json!({"devList":{"target":info}})),
            Err(CentralError::Protocol)
        ));
    }
}

#[test]
fn incomplete_candidates_do_not_reject_the_complete_device_list() {
    let mut bad = access(json!("pending"));
    bad.as_object_mut().unwrap().remove("pushAvailable");
    let response = parse_response(
        &RequestKind::ListAll,
        &json!({"devList":{"good":access(json!("ready")),"bad":bad}}),
    );
    assert!(
        response.is_ok(),
        "a candidate error must not reject the container"
    );
    let Response::Devices(entries) = response.unwrap() else {
        panic!()
    };
    assert!(
        entries
            .iter()
            .find(|entry| entry.did == "good")
            .unwrap()
            .access
            .is_ok()
    );
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.did == "bad")
            .unwrap()
            .access,
        Err(CentralError::Protocol)
    );
    assert!(matches!(
        parse_response(&RequestKind::ListAll, &json!({"devList":{"bad did":{}}})),
        Err(CentralError::Protocol)
    ));
}

#[test]
fn full_device_lists_preserve_names_validate_flags_and_single_did_scope() {
    let value = json!({"devList":{"one":access(json!("插座 一")),"two":access(json!("")),"three":access(Value::Null)}});
    let Response::Devices(devices) = parse_response(&RequestKind::ListAll, &value).unwrap() else {
        panic!()
    };
    assert_eq!(devices.len(), 3);
    assert_eq!(
        devices
            .iter()
            .find(|device| device.did == "one")
            .unwrap()
            .access
            .as_ref()
            .unwrap()
            .name
            .as_deref(),
        Some("插座 一")
    );
    assert!(
        devices
            .iter()
            .filter(|device| device.did != "one")
            .all(|device| device.access.as_ref().unwrap().name.is_none())
    );
    let mut invalid = value.clone();
    invalid["devList"]["two"]["pushAvailable"] = Value::Null;
    let Response::Devices(entries) = parse_response(&RequestKind::ListAll, &invalid).unwrap()
    else {
        panic!()
    };
    assert!(
        entries
            .iter()
            .find(|entry| entry.did == "two")
            .unwrap()
            .access
            .is_err()
    );
    assert!(matches!(
        parse_response(&RequestKind::List { did: "one".into() }, &invalid),
        Ok(Response::Device(Some(_)))
    ));
    let publish = RequestKind::ListAll.publication(1, "reply").unwrap();
    let payload = wire::decode_mips(&publish.payload).unwrap().payload;
    assert!(payload.get("filter").is_none());
    assert_eq!(
        payload["info"],
        json!([
            "name",
            "model",
            "urn",
            "online",
            "specV2Access",
            "pushAvailable"
        ])
    );
}

#[test]
fn filtered_device_requests_preserve_their_payload_and_response_scope() {
    let handle = queued_handle(Timing::default());
    let observed = Rc::new(RefCell::new(None));
    let callback = observed.clone();
    let dids = vec!["one".into(), "missing".into(), "bad".into()];
    let future = handle.get_devices_filtered(&dids, move |result| {
        *callback.borrow_mut() = Some(result);
    });
    let request = handle.shared.queue.borrow_mut().pop_front().unwrap();
    let publish = request.kind.publication(1, "reply").unwrap();
    assert_eq!(publish.topic, "master/proxy/getDevList");
    let payload = wire::decode_mips(&publish.payload).unwrap().payload;
    assert_eq!(payload["filter"]["did"], json!(dids));
    assert_eq!(
        payload["info"],
        json!([
            "name",
            "model",
            "urn",
            "online",
            "specV2Access",
            "pushAvailable"
        ])
    );
    let reply = json!({"devList":{
        "one":access(json!("插座 一")),
        "bad":{},
        "unrelated":access(json!("other")),
        "invalid unrelated did":null
    }});
    (request.complete)(
        parse_response(&request.kind, &reply),
        crate::xiaomi::central::RequestStage::BusinessResult,
        None,
    );
    let entries = block_on(future).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].did, "bad");
    assert_eq!(entries[0].access, Err(CentralError::Protocol));
    assert_eq!(entries[1].did, "one");
    assert_eq!(
        entries[1].access.as_ref().unwrap().name.as_deref(),
        Some("插座 一")
    );
    assert_eq!(*observed.borrow(), Some(Ok(entries)));
}

#[test]
fn empty_or_invalid_device_filters_are_rejected_before_submission() {
    let handle = queued_handle(Timing::default());
    for dids in [vec![], vec!["valid".into(), "invalid did".into()]] {
        let request = handle.get_devices_filtered(&dids, |result| {
            assert_eq!(result, Err(CentralError::InvalidInput));
        });
        assert_eq!(block_on(request), Err(CentralError::InvalidInput));
        assert!(handle.shared.queue.borrow().is_empty());
    }
}

#[test]
fn full_and_single_device_requests_share_the_session_and_keep_their_response_scope() {
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            let Packet::Publish(all) = broker.next().await else {
                panic!()
            };
            assert!(
                wire::decode_mips(&all.payload)
                    .unwrap()
                    .payload
                    .get("filter")
                    .is_none()
            );
            broker.acknowledge(&all).await;
            let Packet::Publish(single) = broker.next().await else {
                panic!()
            };
            assert_eq!(
                wire::decode_mips(&single.payload).unwrap().payload["filter"]["did"],
                json!(["missing"])
            );
            broker.acknowledge(&single).await;
            broker
                .reply(&single, json!({"devList":{"unrelated":null}}), 50)
                .await;
            broker
                .reply(
                    &all,
                    json!({"devList":{"one":access(json!("插座 一")),"two":access(Value::Null)}}),
                    51,
                )
                .await;
            assert!(broker.next_optional().await.is_none());
        },
        |handle, driver| async move {
            let all = handle.get_devices(|result| assert_eq!(result.unwrap().len(), 2));
            let single =
                handle.get_device_list("missing", |result| assert_eq!(result.unwrap(), None));
            let work = async {
                assert_eq!(single.await.unwrap(), None);
                let devices = all.await.unwrap();
                assert_eq!(devices[0].did, "one");
                assert_eq!(
                    devices[0].access.as_ref().unwrap().name.as_deref(),
                    Some("插座 一")
                );
                assert_eq!(devices[1].did, "two");
                handle.close();
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            result.unwrap();
        },
    ));
}
