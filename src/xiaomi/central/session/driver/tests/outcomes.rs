use super::*;

#[test]
fn detailed_outcomes_keep_unsent_queue_and_output_timeouts_unsent() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, budget, written) = rig();
            let write = handle.set_property_detailed("target", BINDING, true, |_| {});
            if dispatched {
                driver.drain_requests().unwrap();
                let past = Instant::now() - Duration::from_secs(1);
                driver.pending.values_mut().next().unwrap().deadline = past;
                driver.output.front_mut().unwrap().deadline = past;
            } else {
                handle
                    .shared
                    .queue
                    .borrow_mut()
                    .front_mut()
                    .unwrap()
                    .deadline = Instant::now() - Duration::from_secs(1);
            }
            driver.expire().unwrap();
            assert_eq!(
                write.await,
                RequestOutcome {
                    result: Err(CentralError::Timeout),
                    stage: RequestStage::NotSent,
                    timeout: Some(RequestTimeout::Queue)
                }
            );
            write_ready(&mut driver, &budget, usize::MAX).await;
            assert_eq!(written.get(), 0);
            assert!(driver.pending.is_empty());
            assert!(driver.output.is_empty());
        }
    });
}

#[test]
fn detailed_outcomes_only_mark_possible_send_after_socket_bytes() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let write = handle.set_property_detailed("target", BINDING, true, |_| {});
        driver.drain_requests().unwrap();
        write_ready(&mut driver, &budget, 0).await;
        assert_eq!(
            driver.pending.values().next().unwrap().stage,
            RequestStage::NotSent
        );
        write_ready(&mut driver, &budget, 3).await;
        assert_eq!(written.get(), 3);
        assert_eq!(
            driver.pending.values().next().unwrap().stage,
            RequestStage::MayHaveBeenSent
        );
        driver.output.front_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
        assert_eq!(driver.expire(), Err(CentralError::Timeout));
        driver.finish_all(CentralError::Timeout);
        assert_eq!(
            write.await,
            RequestOutcome {
                result: Err(CentralError::Timeout),
                stage: RequestStage::MayHaveBeenSent,
                timeout: Some(RequestTimeout::Transport)
            }
        );
        assert_eq!(written.get(), 3);
    });
}

#[test]
fn detailed_outcomes_distinguish_response_timeout_from_transport_and_business_results() {
    block_on(async {
        for case in ["response", "transport", "rejected", "accepted", "malformed"] {
            let (handle, mut driver, budget, _) = rig();
            let callbacks = Rc::new(RefCell::new(Vec::new()));
            let observed = callbacks.clone();
            let write = handle.set_property_detailed("target", BINDING, true, move |outcome| {
                observed.borrow_mut().push(outcome)
            });
            driver.drain_requests().unwrap();
            let mid = *driver.pending.keys().next().unwrap();
            write_ready(&mut driver, &budget, usize::MAX).await;
            let expected = match case {
                "response" => {
                    driver.pending.get_mut(&mid).unwrap().deadline =
                        Instant::now() - Duration::from_secs(1);
                    driver.expire().unwrap();
                    RequestOutcome {
                        result: Err(CentralError::Timeout),
                        stage: RequestStage::MayHaveBeenSent,
                        timeout: Some(RequestTimeout::Response),
                    }
                }
                "transport" => {
                    driver.publications.values_mut().next().unwrap().deadline =
                        Instant::now() - Duration::from_secs(1);
                    assert_eq!(driver.expire(), Err(CentralError::Timeout));
                    driver.finish_all(CentralError::Timeout);
                    RequestOutcome {
                        result: Err(CentralError::Timeout),
                        stage: RequestStage::MayHaveBeenSent,
                        timeout: Some(RequestTimeout::Transport),
                    }
                }
                "rejected" => {
                    reply(&mut driver, mid, json!({"error":{"code":-704042011}}));
                    RequestOutcome {
                        result: Err(CentralError::Rejected(-704042011)),
                        stage: RequestStage::BusinessResult,
                        timeout: None,
                    }
                }
                "accepted" => {
                    reply(
                        &mut driver,
                        mid,
                        json!({"result":[{"did":"target","code":0}]}),
                    );
                    RequestOutcome {
                        result: Ok(()),
                        stage: RequestStage::BusinessResult,
                        timeout: None,
                    }
                }
                _ => {
                    reply(
                        &mut driver,
                        mid,
                        json!({"result":[{"did":"foreign","code":0}]}),
                    );
                    RequestOutcome {
                        result: Err(CentralError::Protocol),
                        stage: RequestStage::MayHaveBeenSent,
                        timeout: None,
                    }
                }
            };
            assert_eq!(
                callbacks.borrow().as_slice(),
                std::slice::from_ref(&expected)
            );
            assert_eq!(write.await, expected);
            reply(
                &mut driver,
                mid,
                json!({"result":[{"did":"target","code":0}]}),
            );
            assert_eq!(callbacks.borrow().len(), 1);
        }
    });
}
