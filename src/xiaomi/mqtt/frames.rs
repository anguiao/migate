use super::ProtocolError;
use bytes::BytesMut;
use mqttbytes::v5::{self, *};

pub(in crate::xiaomi) const MAX_FRAME_SIZE: usize = 1024 * 1024;

fn invalid(reason: &'static str) -> ProtocolError {
    super::invalid(reason)
}

#[derive(Default)]
pub(in crate::xiaomi) struct Frames {
    bytes: BytesMut,
}

impl Frames {
    pub(in crate::xiaomi) fn push(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        if self.bytes.len() + bytes.len() > MAX_FRAME_SIZE + 4096 {
            return Err(invalid("receive buffer limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub(in crate::xiaomi) fn next_packet(&mut self) -> Result<Option<Packet>, ProtocolError> {
        if let Some(first) = self.bytes.first() {
            let packet_type = first >> 4;
            let flags = first & 0x0f;
            if !(1..=14).contains(&packet_type)
                || (packet_type != 3
                    && flags
                        != if matches!(packet_type, 6 | 8 | 10) {
                            2
                        } else {
                            0
                        })
            {
                return Err(invalid("invalid fixed header"));
            }
        }
        let header = match mqttbytes::check(self.bytes.iter(), MAX_FRAME_SIZE) {
            Ok(header) => header,
            Err(mqttbytes::Error::InsufficientBytes(_)) => return Ok(None),
            Err(_) => return Err(invalid("invalid frame length")),
        };
        match header.packet_type().map_err(|_| ProtocolError)? {
            mqttbytes::PacketType::PingReq | mqttbytes::PacketType::PingResp
                if header.frame_length() != 2 =>
            {
                return Err(invalid("nonempty ping packet"));
            }
            mqttbytes::PacketType::Disconnect if self.bytes[1] <= 1 => {
                // MQTT 5 permits omitting the reason and property length;
                // mqttbytes 0.6 requires both for DISCONNECT decoding.
                let mut packet = v5::Disconnect::new();
                if self.bytes[1] == 1 {
                    packet.reason_code = self.bytes[2]
                        .try_into()
                        .map_err(|_| invalid("invalid disconnect reason"))?;
                }
                let _ = self.bytes.split_to(header.frame_length());
                return Ok(Some(Packet::Disconnect(packet)));
            }
            _ => {}
        }
        // Framing has already proved completeness. An insufficient body is
        // malformed, even if the decoder reports InsufficientBytes after
        // consuming that frame; it must never masquerade as pending input.
        v5::read(&mut self.bytes, MAX_FRAME_SIZE)
            .map(Some)
            .map_err(|_| invalid("invalid packet body"))
    }
}

pub(in crate::xiaomi) fn packet_context(packet: &Packet) -> (&'static str, Option<u16>) {
    match packet {
        Packet::Connect(_) => ("CONNECT", None),
        Packet::ConnAck(_) => ("CONNACK", None),
        Packet::Publish(packet) => ("PUBLISH", Some(packet.pkid)),
        Packet::PubAck(packet) => ("PUBACK", Some(packet.pkid)),
        Packet::PubRec(packet) => ("PUBREC", Some(packet.pkid)),
        Packet::PubRel(packet) => ("PUBREL", Some(packet.pkid)),
        Packet::PubComp(packet) => ("PUBCOMP", Some(packet.pkid)),
        Packet::Subscribe(packet) => ("SUBSCRIBE", Some(packet.pkid)),
        Packet::SubAck(packet) => ("SUBACK", Some(packet.pkid)),
        Packet::Unsubscribe(packet) => ("UNSUBSCRIBE", Some(packet.pkid)),
        Packet::UnsubAck(packet) => ("UNSUBACK", Some(packet.pkid)),
        Packet::PingReq => ("PINGREQ", None),
        Packet::PingResp => ("PINGRESP", None),
        Packet::Disconnect(_) => ("DISCONNECT", None),
    }
}

pub(in crate::xiaomi) fn encode_packet(packet: &Packet) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = BytesMut::new();
    let result = match packet {
        Packet::Connect(packet) => packet.write(&mut bytes),
        Packet::ConnAck(packet) => {
            let mut packet = packet.clone();
            if packet.properties.is_none() {
                packet.properties = Some(ConnAckProperties::new());
            }
            packet.write(&mut bytes)
        }
        Packet::Publish(packet) => packet.write(&mut bytes),
        Packet::PubAck(packet) => packet.write(&mut bytes),
        Packet::PubRec(packet) => packet.write(&mut bytes),
        Packet::PubRel(packet) => packet.write(&mut bytes),
        Packet::PubComp(packet) => packet.write(&mut bytes),
        Packet::Subscribe(packet) => packet.write(&mut bytes),
        Packet::SubAck(packet) => packet.write(&mut bytes),
        Packet::Unsubscribe(packet) => packet.write(&mut bytes),
        Packet::UnsubAck(packet) => packet.write(&mut bytes),
        Packet::Disconnect(packet) => packet.write(&mut bytes),
        Packet::PingReq => {
            bytes.extend_from_slice(&[0xc0, 0]);
            Ok(2)
        }
        Packet::PingResp => {
            bytes.extend_from_slice(&[0xd0, 0]);
            Ok(2)
        }
    };
    result.map_err(|_| ProtocolError)?;
    Ok(bytes.to_vec())
}
