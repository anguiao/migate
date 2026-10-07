use super::*;

struct ControlledIo {
    budget: Rc<Cell<usize>>,
    written: Rc<Cell<usize>>,
}
impl AsyncRead for ControlledIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
}
impl AsyncWrite for ControlledIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let count = bytes.len().min(self.budget.get());
        if count == 0 {
            return Poll::Pending;
        }
        self.budget.set(self.budget.get() - count);
        self.written.set(self.written.get() + count);
        Poll::Ready(Ok(count))
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_close(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
pub(super) fn rig() -> (
    SessionHandle,
    SessionDriver,
    Rc<Cell<usize>>,
    Rc<Cell<usize>>,
) {
    let budget = Rc::new(Cell::new(0));
    let written = Rc::new(Cell::new(0));
    let (handle, mut driver) = SessionDriver::new(
        Box::new(ControlledIo {
            budget: budget.clone(),
            written: written.clone(),
        }),
        wire::Frames::default(),
        "client",
        Timing::default(),
        wire::MAX_FRAME_SIZE,
        LIMIT,
    )
    .unwrap();
    driver.subscriptions.clear();
    driver.output.clear();
    driver.ready = true;
    (handle, driver, budget, written)
}
pub(super) async fn write_ready(driver: &mut SessionDriver, budget: &Cell<usize>, count: usize) {
    budget.set(count);
    let _ = poll_once(future::poll_fn(|cx| driver.poll_io(cx))).await;
}
pub(super) fn reply(driver: &mut SessionDriver, mid: u32, value: serde_json::Value) {
    driver
        .packet(
            Packet::Publish(Publish::new(
                "client/reply",
                QoS::AtMostOnce,
                wire::encode_mips(mid, None, &value),
            )),
            &mut |_| {},
        )
        .unwrap();
}
pub(super) const BINDING: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };

pub(super) fn received_property(did: &str, pkid: u16, value: bool) -> Packet {
    let mut publish = Publish::new(
        format!("client/appMsg/notify/iot/{did}/property/2.1"),
        QoS::ExactlyOnce,
        wire::encode_mips(1, None, &json!({"did":did,"siid":2,"piid":1,"value":value})),
    );
    publish.pkid = pkid;
    Packet::Publish(publish)
}

pub(super) async fn ready_property(handle: &SessionHandle, driver: &mut SessionDriver, did: &str) {
    let subscribed = handle.subscribe_property(did, BINDING);
    driver.drain_requests().unwrap();
    let pkid = *driver.subscriptions.keys().next().unwrap();
    driver
        .packet(
            Packet::SubAck(SubAck::new(pkid, vec![SubscribeReasonCode::QoS2])),
            &mut |_| {},
        )
        .unwrap();
    subscribed.await.unwrap();
    driver.output.clear();
}

pub(super) fn hooks(
    allowed: Rc<Cell<bool>>,
    checks: Rc<Cell<usize>>,
    first_send: Rc<Cell<usize>>,
) -> SendHooks {
    SendHooks::new(
        move || {
            checks.set(checks.get() + 1);
            if allowed.get() {
                SendDecision::Send
            } else {
                SendDecision::Withdraw
            }
        },
        move || first_send.set(first_send.get() + 1),
    )
}

pub(super) fn unsent<T>(error: CentralError) -> RequestOutcome<T> {
    RequestOutcome {
        result: Err(error),
        stage: RequestStage::NotSent,
        timeout: None,
    }
}

pub(super) fn count_result<T>(calls: &Rc<Cell<usize>>) -> impl FnOnce(RequestOutcome<T>) + use<T> {
    let calls = calls.clone();
    move |_| calls.set(calls.get() + 1)
}

pub(super) struct PendingIo;
impl AsyncRead for PendingIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
}
impl AsyncWrite for PendingIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
    fn poll_close(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl SessionDriver {
    pub(crate) fn ready_for_test(
        stream: impl AsyncRead + AsyncWrite + Unpin + 'static,
        request: std::time::Duration,
    ) -> (SessionHandle, Self) {
        let (handle, mut driver) = Self::new(
            Box::new(stream),
            wire::Frames::default(),
            "controlled",
            Timing {
                request,
                keepalive: None,
            },
            wire::MAX_FRAME_SIZE,
            LIMIT,
        )
        .unwrap();
        driver.subscriptions.clear();
        driver.output.clear();
        driver.ready = true;
        (handle, driver)
    }
}
