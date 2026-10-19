use super::*;

#[test]
fn short_writes_preserve_offsets_and_partial_timeout_closes_without_replay() {
    for stalls in [false, true] {
        let mode = Rc::new(Cell::new(0));
        let control = mode.clone();
        let written = Rc::new(RefCell::new(Vec::new()));
        let bytes = written.clone();
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let first = callbacks.clone();
        let second = callbacks.clone();
        block_on(pair_io(
            move |mut broker| async move {
                broker.setup().await;
                broker.subscription().await;
                if stalls {
                    let mut prefix = [0; 3];
                    broker.stream.read_exact(&mut prefix).await.unwrap();
                    assert_eq!(prefix[0], 0x34);
                    let mut extra = [0; 4096];
                    assert!(!matches!(broker.stream.read(&mut extra).await,Ok(len) if len>0));
                } else {
                    for pkid in [60, 61] {
                        let Packet::Publish(write) = broker.next().await else {
                            panic!("whole write expected")
                        };
                        broker.acknowledge(&write).await;
                        broker
                            .reply(&write, json!({"result":[{"did":"target","code":0}]}), pkid)
                            .await;
                    }
                    async_io::Timer::after(Duration::from_millis(20)).await;
                }
            },
            move |handle, driver| async move {
                let work = async {
                    handle
                        .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                        .await
                        .unwrap();
                    control.set(if stalls { 2 } else { 1 });
                    let first_write = handle.set_property(
                        "target",
                        PropertyBinding { siid: 2, piid: 1 },
                        true,
                        move |outcome| first.borrow_mut().push(outcome.result),
                    );
                    let second_write = handle.set_property(
                        "target",
                        PropertyBinding { siid: 2, piid: 1 },
                        false,
                        move |outcome| second.borrow_mut().push(outcome.result),
                    );
                    let (first, second) = future::zip(first_write, second_write).await;
                    if stalls {
                        assert_eq!(first.result, Err(CentralError::Timeout));
                        assert_eq!(second.result, Err(CentralError::Timeout));
                    } else {
                        first.result.unwrap();
                        second.result.unwrap();
                        handle.close();
                    }
                };
                let (result, ()) = future::zip(driver.run(|_| {}), work).await;
                if stalls {
                    assert_eq!(result, Err(CentralError::Timeout));
                } else {
                    result.unwrap();
                }
            },
            Timing {
                request: Duration::from_millis(200),
                keepalive: None,
            },
            move |inner| {
                Box::new(ShortIo {
                    inner,
                    mode,
                    written: bytes,
                })
            },
        ));
        if stalls {
            assert_eq!(written.borrow().len(), 3);
            assert_eq!(
                *callbacks.borrow(),
                [Err(CentralError::Timeout), Err(CentralError::Timeout)]
            );
        } else {
            let mut frames = Frames::default();
            frames.push(&written.borrow()).unwrap();
            let mut publishes = 0;
            while let Some(packet) = frames.next_packet().unwrap() {
                if matches!(packet, Packet::Publish(_)) {
                    publishes += 1;
                }
            }
            assert_eq!(publishes, 2);
            assert_eq!(*callbacks.borrow(), [Ok(()), Ok(())]);
        }
    }
}

#[test]
fn write_gate_opening_at_deadline_does_not_send_expired_request() {
    let opens = Rc::new(Cell::new(None));
    let gate = opens.clone();
    let written = Rc::new(Cell::new(0));
    let count = written.clone();
    block_on(pair_io(
        |mut broker| async move {
            broker.setup().await;
            broker.subscription().await;
            let mut bytes = [0; 4096];
            assert!(!matches!(broker.stream.read(&mut bytes).await,Ok(len) if len>0));
        },
        move |handle, driver| async move {
            let work = async {
                handle
                    .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                    .await
                    .unwrap();
                gate.set(Some(Instant::now() + Duration::from_millis(30)));
                let mut timed = handle.clone();
                timed.timing.request = Duration::from_millis(30);
                assert_eq!(
                    timed
                        .set_property(
                            "target",
                            PropertyBinding { siid: 2, piid: 1 },
                            true,
                            |outcome| assert_eq!(outcome.result, Err(CentralError::Timeout))
                        )
                        .await
                        .result,
                    Err(CentralError::Timeout)
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
        move |inner| {
            Box::new(GateIo {
                inner,
                opens,
                written: count,
            })
        },
    ));
    assert_eq!(written.get(), 0);
}
