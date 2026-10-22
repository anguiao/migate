use super::*;

#[test]
fn guarded_write_rechecks_after_pending_and_withdraws_without_sending() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let allowed = Rc::new(Cell::new(true));
        let checks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let observed = callbacks.clone();
        let write = handle.set_property_guarded(
            "target",
            BINDING,
            true,
            hooks(allowed.clone(), checks.clone(), first_send.clone()),
            move |outcome| observed.borrow_mut().push(outcome),
        );
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, 0).await;
        let pending_checks = checks.get();
        assert!(pending_checks > 0);
        assert_eq!(first_send.get(), 0);
        assert_eq!(written.get(), 0);
        assert!(callbacks.borrow().is_empty());
        allowed.set(false);
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert!(checks.get() > pending_checks);
        let expected = unsent(CentralError::Withdrawn);
        assert_eq!(write.await, expected);
        assert_eq!(
            callbacks.borrow().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(first_send.get(), 0);
        assert_eq!(written.get(), 0);
        assert!(driver.output.is_empty());
        assert!(driver.pending.is_empty());
        assert!(driver.publications.is_empty());
        assert_eq!(handle.shared.outstanding.get(), 0);
        reply(
            &mut driver,
            mid,
            json!({"result":[{"did":"target","code":0}]}),
        );
        driver.finish_all(CentralError::Cancelled);
        assert_eq!(callbacks.borrow().len(), 1);
        assert_eq!(handle.shared.outstanding.get(), 0);
    });
}

#[test]
fn guarded_read_rechecks_after_pending_and_withdraws_without_sending() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let allowed = Rc::new(Cell::new(true));
        let checks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let observed = callbacks.clone();
        let read = read_value_guarded(
            &handle,
            "target",
            BINDING,
            Duration::from_secs(6),
            hooks(allowed.clone(), checks.clone(), first_send.clone()),
            move |outcome| observed.borrow_mut().push(outcome),
        );
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, 0).await;
        let pending_checks = checks.get();
        assert!(pending_checks > 0);
        allowed.set(false);
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert!(checks.get() > pending_checks);
        let expected = unsent(CentralError::Withdrawn);
        assert_eq!(read.await, expected);
        assert_eq!(
            callbacks.borrow().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(written.get(), 0);
        assert_eq!(first_send.get(), 0);
        assert!(driver.pending.is_empty());
        assert!(driver.output.is_empty());
        assert!(driver.publications.is_empty());
        assert_eq!(handle.shared.outstanding.get(), 0);
        reply(&mut driver, mid, json!({"value":false}));
        driver.finish_all(CentralError::Cancelled);
        assert_eq!(callbacks.borrow().len(), 1);
    });
}

#[test]
fn guarded_write_locks_the_decision_and_reports_first_send_once() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let allowed = Rc::new(Cell::new(true));
        let checks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let observed = callbacks.clone();
        let write = handle.set_property_guarded(
            "target",
            BINDING,
            true,
            hooks(allowed.clone(), checks.clone(), first_send.clone()),
            move |outcome| observed.borrow_mut().push(outcome),
        );
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, 0).await;
        assert_eq!(first_send.get(), 0);
        write_ready(&mut driver, &budget, 3).await;
        assert_eq!(written.get(), 3);
        assert_eq!(first_send.get(), 1);
        assert_eq!(driver.pending[&mid].stage, RequestStage::MayHaveBeenSent);
        let committed_checks = checks.get();
        allowed.set(false);
        write_ready(&mut driver, &budget, 2).await;
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert!(written.get() > 5);
        assert_eq!(checks.get(), committed_checks);
        assert_eq!(first_send.get(), 1);
        assert!(callbacks.borrow().is_empty());
        reply(
            &mut driver,
            mid,
            json!({"result":[{"did":"target","code":0}]}),
        );
        let expected = RequestOutcome {
            result: Ok(()),
            stage: RequestStage::BusinessResult,
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

#[test]
fn guarded_read_locks_the_decision_after_first_bytes() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let allowed = Rc::new(Cell::new(true));
        let checks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let callbacks = Rc::new(Cell::new(0));
        let read = read_value_guarded(
            &handle,
            "target",
            BINDING,
            Duration::from_secs(6),
            hooks(allowed.clone(), checks.clone(), first_send.clone()),
            count_result(&callbacks),
        );
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, 3).await;
        assert_eq!(written.get(), 3);
        assert_eq!(first_send.get(), 1);
        let committed_checks = checks.get();
        allowed.set(false);
        handle.cancel_unsent_business();
        driver.drain_cancellations();
        assert!(driver.pending.contains_key(&mid));
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(checks.get(), committed_checks);
        assert_eq!(first_send.get(), 1);
        assert!(written.get() > 3);
        reply(&mut driver, mid, json!({"value":false}));
        assert_eq!(
            read.await,
            RequestOutcome {
                result: Ok(json!(false)),
                stage: RequestStage::BusinessResult,
                timeout: None,
            }
        );
        assert_eq!(callbacks.get(), 1);
        assert_eq!(handle.shared.outstanding.get(), 0);
    });
}

#[test]
fn a_guard_cancellation_is_cancelled_rather_than_withdrawn() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let callbacks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let sent = first_send.clone();
        let write = handle.set_property_guarded(
            "target",
            BINDING,
            true,
            SendHooks::new(|| SendDecision::Cancel, move || sent.set(sent.get() + 1)),
            count_result(&callbacks),
        );
        driver.drain_requests().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(write.await, unsent(CentralError::Cancelled));
        assert_eq!(callbacks.get(), 1);
        assert_eq!(first_send.get(), 0);
        assert_eq!(written.get(), 0);
        assert_eq!(handle.shared.outstanding.get(), 0);
        assert!(driver.pending.is_empty());
        assert!(driver.publications.is_empty());
        driver.finish_all(CentralError::Cancelled);
        assert_eq!(callbacks.get(), 1);
    });
}
