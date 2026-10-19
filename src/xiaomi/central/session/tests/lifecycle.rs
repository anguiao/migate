use super::*;

#[test]
fn disconnect_and_access_denial_finish_every_pending_callback() {
    for denied in [false, true] {
        let expected = if denied {
            CentralError::Rejected(135)
        } else {
            CentralError::Disconnected
        };
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let first = callbacks.clone();
        let second = callbacks.clone();
        let error = expected.clone();
        block_on(pair(
            move |mut broker| async move {
                broker.setup().await;
                broker
                    .wait_for(|packet| matches!(packet, Packet::Publish(_)))
                    .await;
                if denied {
                    let mut packet = Disconnect::new();
                    packet.reason_code = DisconnectReasonCode::NotAuthorized;
                    broker.send(Packet::Disconnect(packet)).await;
                } else {
                    broker.send(Packet::Disconnect(Disconnect::new())).await;
                }
                async_io::Timer::after(Duration::from_millis(20)).await;
            },
            move |handle, driver| async move {
                let read = handle.read_property_value(
                    "target",
                    PropertyBinding { siid: 2, piid: 1 },
                    move |outcome| first.borrow_mut().push(outcome.result.map(|_| ())),
                );
                let write = handle.set_property(
                    "target",
                    PropertyBinding { siid: 2, piid: 1 },
                    true,
                    move |outcome| second.borrow_mut().push(outcome.result),
                );
                let (result, (read, write)) =
                    future::zip(driver.run(|_| {}), future::zip(read, write)).await;
                assert_eq!(result, Err(error.clone()));
                assert_eq!(read.result, Err(error.clone()));
                assert_eq!(write.result, Err(error));
            },
        ));
        assert_eq!(*callbacks.borrow(), [Err(expected.clone()), Err(expected)]);
    }
}

#[test]
fn dropping_unpolled_driver_completes_queued_callback_and_closes_handle() {
    let callbacks = Rc::new(RefCell::new(Vec::new()));
    let results = callbacks.clone();
    block_on(pair(
        |mut broker| async move {
            assert!(matches!(broker.next().await, Packet::Connect(_)));
            broker
                .send(Packet::ConnAck(ConnAck::new(
                    ConnectReturnCode::Success,
                    false,
                )))
                .await;
            let mut bytes = [0; 4096];
            assert!(!matches!(broker.stream.read(&mut bytes).await,Ok(len) if len>0));
        },
        move |handle, driver| async move {
            let request = handle.set_property(
                "target",
                PropertyBinding { siid: 2, piid: 1 },
                true,
                move |outcome| results.borrow_mut().push(outcome.result),
            );
            drop(driver);
            assert_eq!(request.await.result, Err(CentralError::Cancelled));
            assert_eq!(
                handle
                    .read_property_value(
                        "target",
                        PropertyBinding { siid: 2, piid: 1 },
                        |outcome| assert_eq!(outcome.result, Err(CentralError::Cancelled))
                    )
                    .await
                    .result,
                Err(CentralError::Cancelled)
            );
        },
    ));
    assert_eq!(*callbacks.borrow(), [Err(CentralError::Cancelled)]);
}

#[test]
fn reconnect_never_completes_from_old_session_request_id() {
    block_on(async {
        let fixture = TlsFixture::new(false);
        let old = Rc::new(Cell::new(0));
        let old_id = old.clone();
        for session in 0..2 {
            let server_old = old_id.clone();
            pair_fixture(
                &fixture,
                move |mut broker| async move {
                    broker.setup().await;
                    let Packet::Publish(read) = broker.next().await else {
                        panic!("read expected")
                    };
                    let id = wire::decode_mips(&read.payload).unwrap().id;
                    broker.acknowledge(&read).await;
                    if session == 0 {
                        server_old.set(id);
                    } else {
                        assert_ne!(id, server_old.get());
                        broker
                            .send(Packet::Publish(Publish::new(
                                format!("{}/reply", broker.client_id),
                                QoS::AtMostOnce,
                                wire::encode_mips(server_old.get(), None, &json!({"value":true})),
                            )))
                            .await;
                    }
                    broker.reply(&read, json!({"value":false}), 90).await;
                    async_io::Timer::after(Duration::from_millis(20)).await;
                },
                move |handle, driver| async move {
                    let work = async {
                        assert!(
                            !handle
                                .read_property_value(
                                    "target",
                                    PropertyBinding { siid: 2, piid: 1 },
                                    |outcome| assert_eq!(outcome.result, Ok(json!(false)))
                                )
                                .await
                                .result
                                .unwrap()
                                .as_bool()
                                .unwrap()
                        );
                        handle.close();
                    };
                    let (result, ()) = future::zip(driver.run(|_| {}), work).await;
                    result.unwrap();
                },
                Timing {
                    request: Duration::from_millis(200),
                    keepalive: None,
                },
                |stream| stream,
            )
            .await;
        }
    });
}

#[test]
fn connack_rejection_and_persisted_session_are_rejected_over_tls() {
    for persisted in [false, true] {
        block_on(async {
            let fixture = TlsFixture::new(false);
            let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
            let address = listener.get_ref().local_addr().unwrap();
            let acceptor = TlsAcceptor::from(fixture.server_config());
            let server = async {
                let (socket, _) = listener.accept().await.unwrap();
                let stream = acceptor.accept(socket).await.unwrap();
                let mut broker = Broker {
                    stream,
                    frames: wire::Frames::default(),
                    client_id: String::new(),
                    deferred: VecDeque::new(),
                };
                assert!(matches!(broker.next().await, Packet::Connect(_)));
                broker
                    .send(Packet::ConnAck(ConnAck::new(
                        if persisted {
                            ConnectReturnCode::Success
                        } else {
                            ConnectReturnCode::NotAuthorized
                        },
                        persisted,
                    )))
                    .await;
            };
            let client = async {
                let stream = tls::connect(address, &fixture.credentials, &fixture.ca_pem)
                    .await
                    .unwrap();
                let error = match start(
                    Box::new(stream),
                    &fixture.credentials.virtual_did,
                    Timing::default(),
                )
                .await
                {
                    Ok(_) => panic!("invalid CONNACK accepted"),
                    Err(error) => error,
                };
                assert_eq!(
                    error,
                    if persisted {
                        CentralError::Protocol
                    } else {
                        CentralError::Rejected(135)
                    }
                );
            };
            future::zip(server, client).await;
        });
    }
}
