use super::*;
use crate::xiaomi::mqtt::handshake::Handshake;
use futures_lite::{
    future,
    io::{AsyncRead, AsyncWrite},
};
use mqttbytes::v5::{ConnAck, ConnAckProperties, Connect, ConnectReturnCode, Packet};
use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

struct MockIo {
    reads: VecDeque<Vec<u8>>,
    written: Vec<u8>,
}

impl MockIo {
    fn new(reads: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            reads: reads.into_iter().collect(),
            written: Vec::new(),
        }
    }

    fn connack(
        code: ConnectReturnCode,
        session_present: bool,
        properties: ConnAckProperties,
    ) -> Vec<u8> {
        let mut ack = ConnAck::new(code, session_present);
        ack.properties = Some(properties);
        encode_packet(&Packet::ConnAck(ack)).unwrap()
    }
}

impl AsyncRead for MockIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let Some(chunk) = self.reads.pop_front() else {
            return Poll::Pending;
        };
        assert!(chunk.len() <= bytes.len(), "test chunk exceeds read buffer");
        bytes[..chunk.len()].copy_from_slice(&chunk);
        Poll::Ready(Ok(chunk.len()))
    }
}

impl AsyncWrite for MockIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.written.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn exchange(io: &mut MockIo) -> Result<Handshake, HandshakeError> {
    let mut frames = Frames::default();
    future::block_on(handshake(
        io,
        &mut frames,
        Connect::new("client"),
        Duration::from_millis(50),
    ))
}

#[test]
fn connect_is_written_and_common_connack_facts_are_returned() {
    let mut properties = ConnAckProperties::new();
    properties.receive_max = Some(8);
    properties.max_packet_size = Some(1024);
    properties.max_qos = Some(2);
    properties.server_keep_alive = Some(30);
    let mut io = MockIo::new([MockIo::connack(
        ConnectReturnCode::Success,
        false,
        properties,
    )]);
    let handshake = exchange(&mut io).unwrap();
    assert_eq!(handshake.max_packet, 1024);
    assert_eq!(handshake.receive_max, Some(8));
    assert_eq!(handshake.max_qos, Some(2));
    assert_eq!(handshake.server_keep_alive, Some(30));

    let mut frames = Frames::default();
    frames.push(&io.written).unwrap();
    let Some(Packet::Connect(connect)) = frames.next_packet().unwrap() else {
        panic!("CONNECT expected on the wire");
    };
    assert_eq!(connect.client_id, "client");
}

#[test]
fn a_connack_split_across_reads_is_reassembled() {
    let bytes = MockIo::connack(ConnectReturnCode::Success, false, ConnAckProperties::new());
    let split = bytes.len() / 2;
    let mut io = MockIo::new([bytes[..split].to_vec(), bytes[split..].to_vec()]);
    let handshake = exchange(&mut io).unwrap();
    assert_eq!(handshake.max_packet, MAX_FRAME_SIZE);
    assert_eq!(handshake.receive_max, None);
}

#[test]
fn common_connack_violations_are_protocol_errors() {
    let violations = [
        {
            let mut properties = ConnAckProperties::new();
            properties.receive_max = Some(0);
            properties
        },
        {
            let mut properties = ConnAckProperties::new();
            properties.max_packet_size = Some(0);
            properties
        },
        {
            let mut properties = ConnAckProperties::new();
            properties.authentication_method = Some("method".into());
            properties
        },
        {
            let mut properties = ConnAckProperties::new();
            properties.authentication_data = Some(vec![1, 2].into());
            properties
        },
    ];
    for properties in violations {
        let mut io = MockIo::new([MockIo::connack(
            ConnectReturnCode::Success,
            false,
            properties,
        )]);
        assert_eq!(exchange(&mut io), Err(HandshakeError::Protocol));
    }
    let mut io = MockIo::new([MockIo::connack(
        ConnectReturnCode::Success,
        true,
        ConnAckProperties::new(),
    )]);
    assert_eq!(exchange(&mut io), Err(HandshakeError::Protocol));
}

#[test]
fn a_rejected_connack_keeps_the_code_for_driver_policy() {
    let mut io = MockIo::new([MockIo::connack(
        ConnectReturnCode::BadUserNamePassword,
        false,
        ConnAckProperties::new(),
    )]);
    assert_eq!(
        exchange(&mut io),
        Err(HandshakeError::Rejected(
            ConnectReturnCode::BadUserNamePassword
        ))
    );
}

#[test]
fn non_connack_packets_and_closed_streams_are_distinct_errors() {
    let mut io = MockIo::new([encode_packet(&Packet::PingResp).unwrap()]);
    assert_eq!(exchange(&mut io), Err(HandshakeError::Protocol));

    let mut io = MockIo::new([Vec::new()]);
    assert_eq!(exchange(&mut io), Err(HandshakeError::Disconnected));
}

#[test]
fn a_silent_server_times_out() {
    let mut io = MockIo::new([]);
    let mut frames = Frames::default();
    let result = future::block_on(handshake(
        &mut io,
        &mut frames,
        Connect::new("client"),
        Duration::from_millis(10),
    ));
    assert_eq!(result, Err(HandshakeError::Timeout));
}
