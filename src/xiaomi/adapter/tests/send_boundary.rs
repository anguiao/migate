use super::*;
use crate::xiaomi::{
    adapter::{routing::PathPriority, state::Session},
    central::{
        CentralError, RequestOutcome, RequestStage, RequestTimeout, SessionDriver, SessionHandle,
    },
};
use futures_lite::io::{AsyncRead, AsyncWrite};
use std::{
    cell::Cell,
    collections::VecDeque,
    task::{Context as TaskContext, Poll, Waker},
};

#[derive(Default)]
struct Wire {
    budget: Cell<usize>,
    polls: Cell<usize>,
    written: RefCell<Vec<u8>>,
    incoming: RefCell<VecDeque<u8>>,
    waker: RefCell<Option<Waker>>,
}
impl Wire {
    fn wake(&self) {
        if let Some(waker) = self.waker.borrow_mut().take() {
            waker.wake();
        }
    }
    fn allow(&self, bytes: usize) {
        self.budget.set(bytes);
        self.wake();
    }
    fn accept(&self) {
        self.reply(accepted("one", 2, 0));
    }
    fn reply(&self, value: Value) {
        let mut frames = crate::xiaomi::central::MqttFrames::default();
        frames.push(&self.written.borrow()).unwrap();
        let Packet::Publish(publish) = frames.next_packet().unwrap().unwrap() else {
            panic!("business publication expected")
        };
        assert!(frames.next_packet().unwrap().is_none());
        let mid = decode_mips(&publish.payload).unwrap().id;
        let reply = Publish::new(
            "controlled/reply",
            QoS::AtMostOnce,
            encode_mips(mid, None, &value),
        );
        let mut bytes = bytes::BytesMut::new();
        reply.write(&mut bytes).unwrap();
        self.incoming.borrow_mut().extend(bytes);
        self.wake();
    }
}
struct Transport(Rc<Wire>);
impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut incoming = self.0.incoming.borrow_mut();
        let count = incoming.len().min(bytes.len());
        if count == 0 {
            *self.0.waker.borrow_mut() = Some(cx.waker().clone());
            return Poll::Pending;
        }
        for byte in &mut bytes[..count] {
            *byte = incoming.pop_front().unwrap();
        }
        Poll::Ready(Ok(count))
    }
}
impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.0.polls.set(self.0.polls.get() + 1);
        let count = bytes.len().min(self.0.budget.get());
        if count == 0 {
            *self.0.waker.borrow_mut() = Some(cx.waker().clone());
            return Poll::Pending;
        }
        self.0.budget.set(self.0.budget.get() - count);
        self.0
            .written
            .borrow_mut()
            .extend_from_slice(&bytes[..count]);
        Poll::Ready(Ok(count))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_close(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
fn local(c: &Control, wire: Rc<Wire>) -> (SessionHandle, SessionDriver) {
    let (handle, driver) = SessionDriver::ready_for_test(Transport(wire), Duration::from_secs(1));
    c.context.live.borrow_mut().session = Some(Session::new(
        handle.clone(),
        c.context.refresh_changed.clone(),
    ));
    c.context.local_list_success(&handle);
    c.context
        .observe_access(vec![super::list_scheduling::observation(&c.data[0], true)]);
    c.context.reconcile_all().unwrap();
    (handle, driver)
}
fn response_timeout() -> RequestOutcome<()> {
    RequestOutcome {
        result: Err(CentralError::Timeout),
        stage: RequestStage::MayHaveBeenSent,
        timeout: Some(RequestTimeout::Response),
    }
}

#[test]
fn an_unsent_pending_write_reselects_cloud_and_submits_the_absolute_target_once() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| c.context.cloud_scope_valid.get()).await;
        let wire = Rc::new(Wire::default());
        let (handle, driver) = local(&c, wire.clone());
        let one = function(&c.context.registry, "one", "main");
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver unexpectedly stopped");
            },
            async {
                let mut command = Box::pin(one.execute(Command::On));
                assert!(poll_once(command.as_mut()).await.is_none());
                until(|| wire.polls.get() > 0).await;
                c.context.local_outcome(&handle, &response_timeout());
                wire.allow(usize::MAX);
                assert_eq!(command.await.unwrap().power, Power::On);
                assert!(wire.written.borrow().is_empty());
                let cloud = c.cloud.lock().unwrap();
                assert_eq!(cloud.property_writes.len(), 1);
                assert!(cloud.property_writes[0].3);
            },
        )
        .await;
    }));
}

#[test]
fn write_observation_capture_occurs_at_first_socket_bytes() {
    for after_send in [false, true] {
        block_on(Rig::new(true, &["one"]).run(|c| async move {
            let wire = Rc::new(Wire::default());
            let (_, driver) = local(&c, wire.clone());
            let one = function(&c.context.registry, "one", "main");
            let work = c.context.live.borrow().devices["one"].work.clone().unwrap();
            future::or(
                async {
                    driver.run(|_| {}).await.unwrap();
                    panic!("driver unexpectedly stopped");
                },
                async {
                    let mut command = Box::pin(one.execute(Command::On));
                    assert!(poll_once(command.as_mut()).await.is_none());
                    until(|| wire.polls.get() > 0).await;
                    work.notify(
                        crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                        false,
                    );
                    wire.allow(1);
                    until(|| wire.written.borrow().len() == 1).await;
                    if after_send {
                        // The same value still counts as a notification after the write.
                        work.notify(
                            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                            false,
                        );
                    }
                    wire.allow(usize::MAX);
                    until(|| {
                        let mut frames = crate::xiaomi::central::MqttFrames::default();
                        frames.push(&wire.written.borrow()).unwrap();
                        frames.next_packet().unwrap().is_some()
                    })
                    .await;
                    wire.accept();
                    assert_eq!(
                        command.await.unwrap().power,
                        if after_send { Power::Off } else { Power::On }
                    );
                    assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 0);
                },
            )
            .await;
        }));
    }
}

#[test]
fn only_current_sent_response_timeouts_degrade_and_only_current_lists_restore() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        let (old, _) = local(&c, Rc::new(Wire::default()));
        for (stage, timeout, error) in [
            (
                RequestStage::NotSent,
                Some(RequestTimeout::Queue),
                CentralError::Timeout,
            ),
            (
                RequestStage::MayHaveBeenSent,
                Some(RequestTimeout::Subscription),
                CentralError::Timeout,
            ),
            (
                RequestStage::MayHaveBeenSent,
                Some(RequestTimeout::Transport),
                CentralError::Timeout,
            ),
            (
                RequestStage::BusinessResult,
                None,
                CentralError::Rejected(-1),
            ),
            (RequestStage::NotSent, None, CentralError::Cancelled),
        ] {
            c.context.local_outcome(
                &old,
                &RequestOutcome::<()> {
                    result: Err(error),
                    stage,
                    timeout,
                },
            );
            assert_eq!(c.context.priority(), PathPriority::LocalFirst);
        }
        c.context.local_outcome(&old, &response_timeout());
        assert_eq!(c.context.priority(), PathPriority::CloudFirst);
        c.context.disconnect("test reconnect".into());
        let (new, _) = SessionDriver::ready_for_test(
            Transport(Rc::new(Wire::default())),
            Duration::from_secs(1),
        );
        c.context.live.borrow_mut().session =
            Some(Session::new(new.clone(), c.context.refresh_changed.clone()));
        assert_eq!(c.context.priority(), PathPriority::CloudFirst);
        c.context.local_list_success(&old);
        assert_eq!(c.context.priority(), PathPriority::CloudFirst);
        c.context.local_list_success(&new);
        assert_eq!(c.context.priority(), PathPriority::LocalFirst);
        c.context.local_outcome(&old, &response_timeout());
        assert_eq!(c.context.priority(), PathPriority::LocalFirst);
    }));
}

#[test]
fn a_cancelled_zero_byte_refresh_keeps_one_demand_until_shared_recovery() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        let wire = Rc::new(Wire::default());
        let (handle, driver) = local(&c, wire.clone());
        let one = function(&c.context.registry, "one", "main");
        let work = c.context.live.borrow().devices["one"].work.clone().unwrap();
        let reads = crate::xiaomi::adapter::work::refreshes(c.context.clone(), handle.clone());
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver unexpectedly stopped");
            },
            future::or(
                async {
                    reads.await.unwrap();
                    panic!("refresh task unexpectedly stopped");
                },
                async {
                    until(|| wire.polls.get() > 0).await;
                    c.clock
                        .automatic
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    c.context.local_outcome(&handle, &response_timeout());
                    wire.allow(usize::MAX);
                    for _ in 0..20 {
                        future::yield_now().await;
                    }
                    for _ in 0..3 {
                        work.refresh();
                    }
                    c.clock.advance(Duration::from_secs(3));
                    for _ in 0..20 {
                        future::yield_now().await;
                    }
                    assert!(wire.written.borrow().is_empty());
                    c.context.local_list_success(&handle);
                    for _ in 0..20 {
                        future::yield_now().await;
                    }
                    c.clock.advance(Duration::from_millis(200));
                    until(|| {
                        let mut frames = crate::xiaomi::central::MqttFrames::default();
                        frames.push(&wire.written.borrow()).unwrap();
                        frames.next_packet().unwrap().is_some()
                    })
                    .await;
                    wire.reply(json!({"value":true}));
                    until(|| one.snapshot().power == Power::On).await;
                    c.clock.advance(Duration::from_secs(3));
                    for _ in 0..20 {
                        future::yield_now().await;
                    }
                    let mut frames = crate::xiaomi::central::MqttFrames::default();
                    frames.push(&wire.written.borrow()).unwrap();
                    let Packet::Publish(publish) = frames.next_packet().unwrap().unwrap() else {
                        panic!("read publication expected")
                    };
                    assert_eq!(publish.topic, "master/proxy/get");
                    assert!(frames.next_packet().unwrap().is_none());
                },
            ),
        )
        .await;
    }));
}
