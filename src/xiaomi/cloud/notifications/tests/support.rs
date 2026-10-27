use super::*;

pub(super) const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };

pub(super) fn credentials() -> XiaomiRecord {
    XiaomiRecord {
        uid: "account".into(),
        region: "cn".into(),
        oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
        redirect_uri: "unused".into(),
        tokens: TokenSet {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            expires_at: 1,
            refresh_at: 1,
        },
        virtual_did: "different-local-identity".into(),
        private_key_pem: "unused".into(),
        certificate_pem: "unused".into(),
    }
}

pub(super) struct Broker {
    pub(super) stream: Async<TcpStream>,
    pub(super) frames: mqtt::Frames,
}

impl Broker {
    pub(super) async fn next(&mut self) -> Packet {
        loop {
            if let Some(packet) = self.frames.next_packet().unwrap() {
                return packet;
            }
            let mut bytes = [0; 4096];
            let len = self.stream.read(&mut bytes).await.unwrap();
            assert_ne!(len, 0, "client closed before expected packet");
            self.frames.push(&bytes[..len]).unwrap();
        }
    }
    pub(super) async fn send(&mut self, packet: Packet) {
        self.stream
            .write_all(&mqtt::encode_packet(&packet).unwrap())
            .await
            .unwrap();
        self.stream.flush().await.unwrap();
    }
    pub(super) async fn setup(&mut self) {
        let Packet::Connect(connect) = self.next().await else {
            panic!("CONNECT expected");
        };
        assert_eq!(connect.client_id, "ha.550e8400e29b41d4a716446655440000");
        assert!(connect.clean_session);
        assert_eq!(connect.protocol, mqttbytes::Protocol::V5);
        let login = connect.login.unwrap();
        assert_eq!(login.username, CLIENT_ID.to_string());
        assert_eq!(login.password, "access-secret");
        self.send(Packet::ConnAck(ConnAck::new(
            ConnectReturnCode::Success,
            false,
        )))
        .await;
    }
    pub(super) async fn subscribe(&mut self, expected: &str) -> u16 {
        let Packet::Subscribe(sub) = self.next().await else {
            panic!("SUBSCRIBE expected");
        };
        assert_eq!(sub.filters.len(), 1);
        assert_eq!(sub.filters[0].path, expected);
        assert_eq!(sub.filters[0].qos, QoS::ExactlyOnce);
        sub.pkid
    }
    pub(super) async fn accept(&mut self, pkid: u16) {
        self.send(Packet::SubAck(SubAck {
            pkid,
            return_codes: vec![SubscribeReasonCode::QoS2],
            properties: None,
        }))
        .await;
    }
    pub(super) async fn property(&mut self, value: Value) {
        self.send(Packet::Publish(Publish::new(
            "device/target/up/properties_changed/2/1",
            QoS::AtMostOnce,
            json!({"params":{"siid":2,"piid":1,"value":value}}).to_string(),
        )))
        .await;
    }
}

pub(super) async fn exercise<S: Future<Output = ()>, C: Future<Output = ()>>(
    timing: Timing,
    server: impl FnOnce(Broker) -> S,
    client: impl FnOnce(NotificationHandle, NotificationDriver) -> C,
) {
    let listener = Async::new(TcpListener::bind("127.0.0.1:0").unwrap()).unwrap();
    let address = listener.get_ref().local_addr().unwrap();
    future::race(
        async {
            future::zip(
                async {
                    let (stream, _) = listener.accept().await.unwrap();
                    server(Broker {
                        stream,
                        frames: mqtt::Frames::default(),
                    })
                    .await;
                },
                async {
                    let (handle, driver) =
                        crate::xiaomi::cloud::notification_test_support::connect(
                            address,
                            &credentials(),
                            timing.request,
                            timing.keepalive,
                        )
                        .await
                        .unwrap();
                    client(handle, driver).await;
                },
            )
            .await;
        },
        async {
            async_io::Timer::after(Duration::from_secs(3)).await;
            panic!("notification test stalled");
        },
    )
    .await;
}

pub(super) fn timing() -> Timing {
    Timing {
        request: Duration::from_millis(100),
        keepalive: None,
    }
}

pub(super) fn received_property(pkid: u16, value: bool) -> Packet {
    let mut publish = Publish::new(
        "device/target/up/properties_changed/2/1",
        QoS::ExactlyOnce,
        json!({"params":{"siid":2,"piid":1,"value":value}}).to_string(),
    );
    publish.pkid = pkid;
    Packet::Publish(publish)
}

pub(crate) async fn connect(
    address: SocketAddr,
    credentials: &XiaomiRecord,
    request: Duration,
    keepalive: Option<Duration>,
) -> Result<(NotificationHandle, NotificationDriver), NotificationError> {
    let prepared = PreparedConnection::new(credentials, Timing { request, keepalive })?;
    let stream = Async::<std::net::TcpStream>::connect(address)
        .await
        .map_err(|_| NotificationError::Io)?;
    prepared.start(Box::new(stream)).await
}
