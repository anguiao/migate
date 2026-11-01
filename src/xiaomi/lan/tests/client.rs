use super::*;
use std::thread;

const DID: u64 = 656878122;

fn hello(socket: &UdpSocket, buffer: &mut [u8]) -> SocketAddr {
    let (length, peer) = socket.recv_from(buffer).unwrap();
    assert_eq!(&buffer[..length], &wire::HELLO);
    let mut reply = wire::HELLO;
    reply[4..12].copy_from_slice(&DID.to_be_bytes());
    reply[12..16].copy_from_slice(&42_u32.to_be_bytes());
    socket.send_to(&reply, peer).unwrap();
    peer
}

#[test]
fn only_matching_authenticated_response_settles_request() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let address = socket.local_addr().unwrap();
    let peer_token = token.clone();
    let peer = thread::spawn(move || {
        let mut buffer = [0; 4096];
        hello(&socket, &mut buffer);
        let (len, source) = socket.recv_from(&mut buffer).unwrap();
        let (_, _, data) = wire::decode(&buffer[..len], &peer_token).unwrap();
        let request: Value = serde_json::from_slice(&data).unwrap();
        let id = request["id"].as_u64().unwrap();
        let wrong = json!({"id":id + 1,"result":"wrong"});
        let packet = wire::encode(DID, 43, &peer_token, &serde_json::to_vec(&wrong).unwrap());
        socket.send_to(&packet, source).unwrap();
        let right = json!({"id":id,"result":"correct"});
        let mut packet = wire::encode(DID, 43, &peer_token, &serde_json::to_vec(&right).unwrap());
        *packet.last_mut().unwrap() ^= 1;
        socket.send_to(&packet, source).unwrap();
        let mut payload = serde_json::to_vec(&right).unwrap();
        payload.push(0);
        let packet = wire::encode(DID, 43, &peer_token, &payload);
        socket.send_to(&packet, source).unwrap();
    });
    let client = Client::new(DID, address, token);
    let sent = Cell::new(0);
    assert_eq!(
        future::block_on(client.request_guarded(
            "test",
            json!([]),
            || true,
            || sent.set(sent.get() + 1)
        ))
        .unwrap(),
        json!("correct")
    );
    assert_eq!(sent.get(), 1);
    peer.join().unwrap();
}

#[test]
fn timeout_reports_whether_business_packet_was_sent() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    for respond_to_hello in [false, true] {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let peer = thread::spawn(move || {
            let mut buffer = [0; 4096];
            if respond_to_hello {
                hello(&socket, &mut buffer);
                socket.recv_from(&mut buffer).unwrap();
            } else {
                socket.recv_from(&mut buffer).unwrap();
            }
            thread::sleep(Duration::from_millis(100));
        });
        let client = Client::new(DID, address, token.clone());
        let error = future::block_on(client.request_with_timeout(
            "test",
            json!([]),
            Duration::from_millis(30),
        ))
        .unwrap_err();
        assert_eq!(error.failure, Failure::Timeout);
        assert_eq!(
            error.stage,
            if respond_to_hello {
                SendStage::Sent
            } else {
                SendStage::BeforeSend
            }
        );
        peer.join().unwrap();
    }
}

#[test]
fn late_reply_cannot_settle_a_new_request() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let address = socket.local_addr().unwrap();
    let peer_token = token.clone();
    let peer = thread::spawn(move || {
        let mut buffer = [0; 4096];
        hello(&socket, &mut buffer);
        let (len, _) = socket.recv_from(&mut buffer).unwrap();
        let (_, _, payload) = wire::decode(&buffer[..len], &peer_token).unwrap();
        let first: Value = serde_json::from_slice(&payload).unwrap();
        hello(&socket, &mut buffer);
        let (len, source) = socket.recv_from(&mut buffer).unwrap();
        let (_, _, payload) = wire::decode(&buffer[..len], &peer_token).unwrap();
        let second: Value = serde_json::from_slice(&payload).unwrap();
        let late = json!({"id":first["id"],"result":"late"});
        let packet = wire::encode(DID, 43, &peer_token, &serde_json::to_vec(&late).unwrap());
        socket.send_to(&packet, source).unwrap();
        let current = json!({"id":second["id"],"result":"current"});
        let packet = wire::encode(DID, 43, &peer_token, &serde_json::to_vec(&current).unwrap());
        socket.send_to(&packet, source).unwrap();
    });
    let client = Client::new(DID, address, token);
    let first =
        future::block_on(client.request_with_timeout("test", json!([]), Duration::from_millis(30)))
            .unwrap_err();
    assert_eq!(first.stage, SendStage::Sent);
    assert_eq!(first.failure, Failure::Timeout);
    assert_eq!(
        future::block_on(client.request_with_timeout("test", json!([]), Duration::from_secs(1)))
            .unwrap(),
        json!("current")
    );
    peer.join().unwrap();
}

#[test]
fn withdrawn_after_handshake_never_sends_business() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let address = socket.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let mut buffer = [0; 4096];
        hello(&socket, &mut buffer);
        assert!(socket.recv_from(&mut buffer).is_err());
    });
    let client = Client::new(DID, address, token);
    let checks = Cell::new(0);
    let sent = Cell::new(0);
    let guard = || {
        checks.set(checks.get() + 1);
        checks.get() == 1
    };
    let error = future::block_on(
        client.request_guarded("test", json!([]), guard, || sent.set(sent.get() + 1)),
    )
    .unwrap_err();
    assert_eq!(error.stage, SendStage::BeforeSend);
    assert_eq!(error.failure, Failure::Withdrawn);
    assert_eq!(sent.get(), 0);
    peer.join().unwrap();
}

#[test]
fn request_identifiers_stay_in_the_miot_range() {
    assert_eq!(request_id(0), 1);
    assert_eq!(request_id(0x7fff_ffff), 0x8000_0000);
    assert_eq!(request_id(0x8000_0000), 1);
    assert_eq!(request_id(u32::MAX), 0x8000_0000);
}
