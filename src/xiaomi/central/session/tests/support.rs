use super::*;
use crate::xiaomi::{
    capability::PropertyBinding,
    central::{CentralError, RequestOutcome, SendDecision, SendHooks},
    property::{PropertyReading, PropertyResults},
};
use serde_json::Value;
use std::{future::Future, time::Duration};

pub(super) fn queued_handle(timing: Timing) -> SessionHandle {
    SessionHandle {
        shared: Rc::new(Shared {
            queue: Default::default(),
            event: event_listener::Event::new(),
            closed: Cell::new(false),
            outstanding: Cell::new(0),
            cancellations: Default::default(),
            property_invalidations: Default::default(),
            cancel_unsent_business: Default::default(),
        }),
        timing,
    }
}

pub(crate) struct Broker {
    pub(super) stream: TlsStream<Async<TcpStream>>,
    pub(super) frames: Frames,
    pub(crate) client_id: String,
    pub(super) deferred: VecDeque<Packet>,
}

impl Broker {
    pub(crate) fn new(stream: TlsStream<Async<TcpStream>>) -> Self {
        Self {
            stream,
            frames: Frames::default(),
            client_id: String::new(),
            deferred: VecDeque::new(),
        }
    }

    pub(crate) async fn next(&mut self) -> Packet {
        if let Some(packet) = self.deferred.pop_front() {
            return packet;
        }
        self.next_wire().await
    }
    pub(crate) async fn wait_for(&mut self, matches: impl Fn(&Packet) -> bool) -> Packet {
        if let Some(index) = self.deferred.iter().position(&matches) {
            return self.deferred.remove(index).unwrap();
        }
        loop {
            let packet = self.next_wire().await;
            if matches(&packet) {
                return packet;
            }
            self.deferred.push_back(packet);
        }
    }
    async fn next_wire(&mut self) -> Packet {
        self.next_optional()
            .await
            .expect("client closed before the expected packet")
    }
    pub(crate) async fn next_optional(&mut self) -> Option<Packet> {
        loop {
            if let Some(packet) = self.frames.next_packet().unwrap() {
                return Some(packet);
            }
            let mut bytes = [0; 4096];
            let len = self.stream.read(&mut bytes).await.ok()?;
            if len == 0 {
                return None;
            }
            self.frames.push(&bytes[..len]).unwrap();
        }
    }
    pub(crate) async fn send(&mut self, packet: Packet) {
        self.stream
            .write_all(&encode_packet(&packet).unwrap())
            .await
            .unwrap();
        self.stream.flush().await.unwrap();
    }
    pub(crate) async fn setup(&mut self) {
        assert!(
            self.setup_if_connected().await,
            "CONNECT expected before close"
        );
    }
    pub(crate) async fn setup_if_connected(&mut self) -> bool {
        let Some(packet) = self.next_optional().await else {
            return false;
        };
        let Packet::Connect(connect) = packet else {
            panic!("CONNECT expected")
        };
        assert!(connect.clean_session);
        assert_eq!(connect.protocol, mqttbytes::Protocol::V5);
        assert!(connect.login.is_none());
        self.client_id = connect.client_id;
        self.send(Packet::ConnAck(ConnAck::new(
            ConnectReturnCode::Success,
            false,
        )))
        .await;
        let Some(packet) = self.next_optional().await else {
            return false;
        };
        let Packet::Subscribe(subscribe) = packet else {
            panic!("SUBSCRIBE expected")
        };
        assert_eq!(
            subscribe
                .filters
                .iter()
                .map(|filter| filter.path.as_str())
                .collect::<Vec<_>>(),
            [
                format!("{}/#", self.client_id),
                "master/appMsg/devListChange".into()
            ]
        );
        self.send(Packet::SubAck(SubAck::new(
            subscribe.pkid,
            vec![SubscribeReasonCode::QoS2; 2],
        )))
        .await;
        true
    }
    pub(crate) async fn subscription(&mut self) {
        let Packet::Subscribe(subscribe) = self.next().await else {
            panic!("property SUBSCRIBE expected")
        };
        assert_eq!(
            subscribe.filters[0].path,
            "master/appMsg/notify/iot/target/property/2.1"
        );
        self.send(Packet::SubAck(SubAck::new(
            subscribe.pkid,
            vec![SubscribeReasonCode::QoS2],
        )))
        .await;
    }
    pub(crate) async fn reply(&mut self, outgoing: &Publish, value: Value, pkid: u16) {
        let message = wire::decode_mips(&outgoing.payload).unwrap();
        let mut reply = Publish::new(
            format!("{}/reply", self.client_id),
            QoS::ExactlyOnce,
            wire::encode_mips(message.id, None, &value),
        );
        reply.pkid = pkid;
        self.send(Packet::Publish(reply)).await;
        self.wait_for(|packet|matches!(packet,Packet::PubRec(ack) if ack.pkid==pkid && ack.reason==PubRecReason::Success)).await;
        self.send(Packet::PubRel(PubRel::new(pkid))).await;
        self.wait_for(|packet|matches!(packet,Packet::PubComp(ack) if ack.pkid==pkid && ack.reason==PubCompReason::Success)).await;
    }
}

pub(super) async fn pair<S, C, SF, CF>(server: S, client: C)
where
    S: FnOnce(Broker) -> SF,
    C: FnOnce(SessionHandle, SessionDriver) -> CF,
    SF: std::future::Future<Output = ()>,
    CF: std::future::Future<Output = ()>,
{
    pair_io(
        server,
        client,
        Timing {
            request: Duration::from_millis(200),
            keepalive: Some(Duration::from_secs(1)),
        },
        |stream| stream,
    )
    .await;
}

pub(super) async fn pair_io<S, C, SF, CF, W>(server: S, client: C, timing: Timing, wrap: W)
where
    S: FnOnce(Broker) -> SF,
    C: FnOnce(SessionHandle, SessionDriver) -> CF,
    SF: Future<Output = ()>,
    CF: Future<Output = ()>,
    W: FnOnce(Box<dyn Transport>) -> Box<dyn Transport>,
{
    let fixture = TlsFixture::new(false);
    pair_fixture(&fixture, server, client, timing, wrap).await;
}
pub(super) async fn pair_fixture<S, C, SF, CF, W>(
    fixture: &TlsFixture,
    server: S,
    client: C,
    timing: Timing,
    wrap: W,
) where
    S: FnOnce(Broker) -> SF,
    C: FnOnce(SessionHandle, SessionDriver) -> CF,
    SF: Future<Output = ()>,
    CF: Future<Output = ()>,
    W: FnOnce(Box<dyn Transport>) -> Box<dyn Transport>,
{
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    let address = listener.get_ref().local_addr().unwrap();
    let acceptor = TlsAcceptor::from(fixture.server_config());
    let accept = async {
        let (socket, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(socket).await.unwrap();
        server(Broker {
            stream,
            frames: Frames::default(),
            client_id: String::new(),
            deferred: VecDeque::new(),
        })
        .await;
    };
    let connect = async {
        let stream = tls::connect(address, &fixture.credentials, &fixture.ca_pem)
            .await
            .unwrap();
        let (handle, driver) = start(
            wrap(Box::new(stream)),
            &fixture.credentials.virtual_did,
            timing,
        )
        .await
        .unwrap();
        client(handle, driver).await;
    };
    future::race(
        async {
            future::zip(accept, connect).await;
        },
        async {
            async_io::Timer::after(Duration::from_secs(3)).await;
            panic!("isolated broker fixture timed out")
        },
    )
    .await;
}

impl Broker {
    pub(crate) async fn acknowledge(&mut self, publish: &Publish) {
        self.send(Packet::PubRec(PubRec::new(publish.pkid))).await;
        self.wait_for(|packet| matches!(packet,Packet::PubRel(ack) if ack.pkid==publish.pkid))
            .await;
        self.send(Packet::PubComp(PubComp::new(publish.pkid))).await;
    }
    pub(crate) fn business(&self, publish: &Publish, value: Value) -> Packet {
        Packet::Publish(Publish::new(
            format!("{}/reply", self.client_id),
            QoS::AtMostOnce,
            wire::encode_mips(
                wire::decode_mips(&publish.payload).unwrap().id,
                None,
                &value,
            ),
        ))
    }
    pub(crate) async fn batch(&mut self, packets: Vec<Packet>) {
        let bytes = packets
            .iter()
            .flat_map(|packet| encode_packet(packet).unwrap())
            .collect::<Vec<_>>();
        self.stream.write_all(&bytes).await.unwrap();
        self.stream.flush().await.unwrap();
    }
}

pub(super) struct ShortIo {
    pub(super) inner: Box<dyn Transport>,
    pub(super) mode: Rc<Cell<u8>>,
    pub(super) written: Rc<RefCell<Vec<u8>>>,
}
impl AsyncRead for ShortIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, bytes)
    }
}
impl AsyncWrite for ShortIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mode = self.mode.get();
        if mode == 3 {
            return Poll::Pending;
        }
        let bytes = if mode == 0 {
            bytes
        } else {
            &bytes[..bytes.len().min(3)]
        };
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if let Poll::Ready(Ok(len)) = result {
            if mode > 0 {
                self.written.borrow_mut().extend_from_slice(&bytes[..len]);
            }
            if mode == 2 {
                self.mode.set(3);
            }
        }
        result
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

pub(super) struct GateIo {
    pub(super) inner: Box<dyn Transport>,
    pub(super) opens: Rc<Cell<Option<Instant>>>,
    pub(super) written: Rc<Cell<usize>>,
}
impl AsyncRead for GateIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, bytes)
    }
}
impl AsyncWrite for GateIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self
            .opens
            .get()
            .is_some_and(|instant| Instant::now() < instant)
        {
            return Poll::Pending;
        }
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if self.opens.get().is_some()
            && let Poll::Ready(Ok(count)) = result
        {
            self.written.set(self.written.get() + count);
        }
        result
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

pub(super) fn access(name: Value) -> Value {
    json!({"name":name,"model":"cuco.plug.v3","urn":"outlet","online":true,"specV2Access":true,"pushAvailable":true})
}

pub(super) fn notification(client_id: &str, did: &str, value: bool) -> Packet {
    Packet::Publish(Publish::new(
        format!("{client_id}/appMsg/notify/iot/{did}/property/2.1"),
        QoS::AtMostOnce,
        wire::encode_mips(1, None, &json!({"did":did,"siid":2,"piid":1,"value":value})),
    ))
}

impl SessionHandle {
    pub(crate) fn read_property_value<F: FnOnce(RequestOutcome<Value>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<Value>> + use<F> {
        self.read_property_value_with_timeout(did, binding, self.timing.request, on_result)
    }
    pub(crate) fn read_property_value_with_timeout<F: FnOnce(RequestOutcome<Value>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        timeout: Duration,
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<Value>> + use<F> {
        let read = self.read_property_observation_with_timeout_guarded(
            did,
            binding,
            timeout,
            SendHooks::new(|| SendDecision::Send, || {}),
            move |outcome| on_result(value_outcome(outcome)),
        );
        async move { value_outcome(read.await) }
    }
    pub(crate) fn set_property<F: FnOnce(RequestOutcome<()>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        value: bool,
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<()>> + use<F> {
        self.set_property_guarded(
            did,
            binding,
            value,
            SendHooks::new(|| SendDecision::Send, || {}),
            on_result,
        )
    }
    pub(crate) fn set_properties<
        F: FnOnce(RequestOutcome<PropertyResults<(), CentralError>>) + 'static,
    >(
        &self,
        did: &str,
        properties: &[(PropertyBinding, Value)],
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<PropertyResults<(), CentralError>>> + use<F> {
        self.set_properties_guarded(
            did,
            properties,
            SendHooks::new(|| SendDecision::Send, || {}),
            on_result,
        )
    }
}

fn value_outcome(outcome: RequestOutcome<PropertyReading>) -> RequestOutcome<Value> {
    RequestOutcome {
        result: outcome.result.map(|reading| reading.value),
        stage: outcome.stage,
        timeout: outcome.timeout,
    }
}
