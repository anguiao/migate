use super::*;

#[test]
fn notification_handle_connection_identity_is_shared_only_by_clones() {
    let make = || {
        NotificationDriver::new(
            Box::new(futures_lite::io::Cursor::new(Vec::new())),
            mqtt::Frames::default(),
            "account".into(),
            timing(),
            mqtt::MAX_FRAME_SIZE,
        )
    };
    let (one, _driver) = make();
    let (two, _other_driver) = make();
    assert!(one.same_connection(&one.clone()));
    assert!(!one.same_connection(&two));
}

#[test]
fn notification_connect_suback_and_messages_follow_wire_order() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        Timing::default(),
        move |mut broker| async move {
            let Packet::Connect(connect) = broker.next().await else {
                panic!();
            };
            assert_eq!(connect.keep_alive, 60);
            assert_eq!(connect.client_id, "ha.550e8400e29b41d4a716446655440000");
            assert_eq!(connect.login.unwrap().username, CLIENT_ID.to_string());
            assert_eq!(connect.properties.unwrap().session_expiry_interval, Some(0));
            broker
                .send(Packet::ConnAck(ConnAck::new(
                    ConnectReturnCode::Success,
                    false,
                )))
                .await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.property(json!(false)).await;
            broker.accept(pkid).await;
            let pkid = broker.subscribe("device/target/state/#").await;
            broker.accept(pkid).await;
            broker.property(json!(true)).await;
            broker
                .send(Packet::Publish(Publish::new(
                    "device/target/state/online",
                    QoS::AtMostOnce,
                    json!({"device_id":"target","event":"online","uid":"account"}).to_string(),
                )))
                .await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let work = async {
                handle.subscribe_property("target", POWER).await.unwrap();
                handle.subscribe_online("target").await.unwrap();
            };
            let drive = driver.run(move |event| {
                collected.borrow_mut().push(event);
                if collected.borrow().len() == 2 {
                    finished.send(()).unwrap();
                    stop.close();
                }
            });
            let (result, ()) = future::zip(drive, work).await;
            result.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [
            NotificationEvent::Property {
                did: "target".into(),
                binding: POWER,
                value: true
            },
            NotificationEvent::Online {
                did: "target".into(),
                online: true
            }
        ]
    );
}

#[test]
fn default_notification_timing_matches_protocol_bounds() {
    assert_eq!(Timing::default().request, Duration::from_secs(10));
    assert_eq!(Timing::default().keepalive, Some(Duration::from_secs(60)));
}

#[test]
fn cloud_connect_authentication_rejections_are_sanitized() {
    for code in [
        ConnectReturnCode::BadUserNamePassword,
        ConnectReturnCode::NotAuthorized,
    ] {
        future::block_on(async {
            let listener = Async::new(TcpListener::bind("127.0.0.1:0").unwrap()).unwrap();
            let address = listener.get_ref().local_addr().unwrap();
            future::zip(
                async {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut broker = Broker {
                        stream,
                        frames: mqtt::Frames::default(),
                    };
                    let Packet::Connect(connect) = broker.next().await else {
                        panic!();
                    };
                    assert_eq!(connect.login.unwrap().password, "access-secret");
                    broker
                        .send(Packet::ConnAck(ConnAck::new(code, false)))
                        .await;
                },
                async {
                    let stream = Async::<TcpStream>::connect(address).await.unwrap();
                    let error = start(Box::new(stream), &credentials(), timing())
                        .await
                        .err()
                        .unwrap();
                    assert_eq!(error, NotificationError::Unauthorized);
                    assert!(!format!("{error:?} {error}").contains("secret"));
                },
            )
            .await;
        });
    }
    for field in ["region", "uid", "uuid", "token"] {
        let mut record = credentials();
        match field {
            "region" => record.region = "us".into(),
            "uid" => record.uid.clear(),
            "uuid" => record.oauth_client_uuid = "invalid".into(),
            "token" => record.tokens.access_token = "token\n".into(),
            _ => unreachable!(),
        }
        let error = future::block_on(start(
            Box::new(futures_lite::io::Cursor::new(Vec::new())),
            &record,
            timing(),
        ))
        .err()
        .unwrap();
        assert_eq!(error, NotificationError::InvalidInput);
    }
}

#[test]
fn keepalive_responses_reset_the_wait_and_silence_ends_the_connection() {
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        Timing {
            request: Duration::from_millis(25),
            keepalive: Some(Duration::from_millis(20)),
        },
        move |mut broker| async move {
            broker.setup().await;
            assert!(matches!(broker.next().await, Packet::PingReq));
            broker.send(Packet::PingResp).await;
            assert!(matches!(broker.next().await, Packet::PingReq));
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            assert_eq!(driver.run(|_| {}).await, Err(NotificationError::Timeout));
            assert_eq!(
                handle.subscribe_online("target").await,
                Err(NotificationError::Cancelled)
            );
            finished.send(()).unwrap();
        },
    ));
}
