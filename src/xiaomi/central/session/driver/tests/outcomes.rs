use super::*;

#[test]
fn detailed_outcomes_keep_unsent_queue_and_output_timeouts_unsent() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, budget, written) = rig();
            let write = handle.set_property("target", BINDING, true, |_| {});
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
        let write = handle.set_property("target", BINDING, true, |_| {});
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
            let write = handle.set_property("target", BINDING, true, move |outcome| {
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

/// Accepts every write and can fail the following flush.
struct FlushFailure {
    accepted: Rc<Cell<usize>>,
    fail_flush: Rc<Cell<bool>>,
}

impl AsyncRead for FlushFailure {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Pending
    }
}

impl AsyncWrite for FlushFailure {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.accepted.set(self.accepted.get() + bytes.len());
        std::task::Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.fail_flush.get() {
            std::task::Poll::Ready(Err(std::io::Error::other("flush failed")))
        } else {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[test]
fn accepted_bytes_mark_sent_even_when_the_same_poll_flush_fails() {
    block_on(async {
        let accepted = Rc::new(Cell::new(0));
        let fail_flush = Rc::new(Cell::new(true));
        let (handle, mut driver) = SessionDriver::ready_for_test(
            FlushFailure {
                accepted: accepted.clone(),
                fail_flush,
            },
            Duration::from_secs(6),
        );
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let observed = callbacks.clone();
        let write = handle.set_property("target", BINDING, true, move |outcome| {
            observed.borrow_mut().push(outcome)
        });
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();

        // The accepted bytes are marked before the same poll reports the
        // flush failure, so the request can never be mistaken for unsent.
        assert_eq!(
            poll_once(future::poll_fn(|cx| driver.poll_io(cx))).await,
            Some(Err(CentralError::Io))
        );
        assert!(accepted.get() > 0);
        assert_eq!(driver.pending[&mid].stage, RequestStage::MayHaveBeenSent);

        driver.finish_all(CentralError::Io);
        let expected = RequestOutcome {
            result: Err(CentralError::Io),
            stage: RequestStage::MayHaveBeenSent,
            timeout: None,
        };
        assert_eq!(write.await, expected);
        assert_eq!(
            callbacks.borrow().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(handle.shared.outstanding.get(), 0);
    });
}

/// Accepts no bytes at all.
struct ZeroWrite;

impl AsyncRead for ZeroWrite {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Pending
    }
}

impl AsyncWrite for ZeroWrite {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Ok(0))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[test]
fn a_zero_byte_transport_write_ends_the_session_as_io() {
    block_on(async {
        let (handle, mut driver) = SessionDriver::ready_for_test(ZeroWrite, Duration::from_secs(6));
        let write = handle.set_property("target", BINDING, true, |_| {});
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        assert_eq!(
            poll_once(future::poll_fn(|cx| driver.poll_io(cx))).await,
            Some(Err(CentralError::Io))
        );
        assert_eq!(
            driver.pending[&mid].stage,
            RequestStage::NotSent,
            "no bytes were accepted"
        );
        driver.finish_all(CentralError::Io);
        assert_eq!(
            write.await,
            RequestOutcome {
                result: Err(CentralError::Io),
                stage: RequestStage::NotSent,
                timeout: None,
            }
        );
        assert_eq!(handle.shared.outstanding.get(), 0);
    });
}
