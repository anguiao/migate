use super::CentralError;
use bytes::BytesMut;
use mqttbytes::v5::{self, *};
use serde_json::Value;

pub(super) const MAX_FRAME_SIZE: usize = 1024 * 1024;

#[derive(Debug)]
pub struct MipsMessage {
    pub id: u32,
    pub payload: Value,
}

pub fn encode_mips(id: u32, reply_topic: Option<&str>, payload: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    field(&mut bytes, 0, &id.to_le_bytes());
    string_field(&mut bytes, 3, "local");
    if let Some(topic) = reply_topic {
        string_field(&mut bytes, 1, topic);
    }
    string_field(&mut bytes, 2, &payload.to_string());
    bytes
}

fn field(bytes: &mut Vec<u8>, tag: u8, value: &[u8]) {
    bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
    bytes.push(tag);
    bytes.extend_from_slice(value);
}

fn string_field(bytes: &mut Vec<u8>, tag: u8, value: &str) {
    bytes.extend_from_slice(&((value.len() + 1) as u32).to_le_bytes());
    bytes.push(tag);
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
}

pub fn decode_mips(mut bytes: &[u8]) -> Result<MipsMessage, CentralError> {
    let mut id = None;
    let mut payload = None;
    let mut seen = [false; 4];
    while !bytes.is_empty() {
        if bytes.len() < 5 {
            return Err(malformed("MIPS", "truncated TLV header"));
        }
        let length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let tag = bytes[4] as usize;
        bytes = &bytes[5..];
        if length > bytes.len() {
            return Err(malformed("MIPS", "truncated TLV value"));
        }
        let value = &bytes[..length];
        bytes = &bytes[length..];
        if tag >= seen.len() {
            continue;
        }
        if seen[tag] {
            return Err(malformed("MIPS", "duplicate field"));
        }
        seen[tag] = true;
        if tag == 0 {
            id = Some(u32::from_le_bytes(
                value
                    .try_into()
                    .map_err(|_| malformed("MIPS", "invalid message id"))?,
            ));
        } else {
            // The TLV length bounds the string; gateways may omit its terminator.
            let value = value.strip_suffix(&[0]).unwrap_or(value);
            if value.contains(&0) {
                return Err(malformed("MIPS", "embedded NUL"));
            }
            let text =
                std::str::from_utf8(value).map_err(|_| malformed("MIPS", "invalid UTF-8"))?;
            if tag == 2 {
                payload = Some(
                    serde_json::from_str(text).map_err(|_| malformed("MIPS", "invalid JSON"))?,
                );
            }
        }
    }
    Ok(MipsMessage {
        id: id.ok_or_else(|| malformed("MIPS", "missing message id"))?,
        payload: payload.ok_or_else(|| malformed("MIPS", "missing payload"))?,
    })
}

fn malformed(layer: &'static str, reason: &'static str) -> CentralError {
    // Never include untrusted topics, properties or payloads in diagnostics.
    log::debug!("Invalid gateway message: layer={layer}, reason={reason}");
    CentralError::Protocol
}

#[derive(Default)]
pub struct Frames {
    bytes: BytesMut,
}

impl Frames {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), CentralError> {
        if self.bytes.len() + bytes.len() > MAX_FRAME_SIZE + 4096 {
            return Err(malformed("MQTT", "receive buffer limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub fn next_packet(&mut self) -> Result<Option<Packet>, CentralError> {
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
                return Err(malformed("MQTT", "invalid fixed header"));
            }
        }
        let header = match mqttbytes::check(self.bytes.iter(), MAX_FRAME_SIZE) {
            Ok(header) => header,
            Err(mqttbytes::Error::InsufficientBytes(_)) => return Ok(None),
            Err(_) => return Err(malformed("MQTT", "invalid frame length")),
        };
        match header.packet_type().map_err(|_| CentralError::Protocol)? {
            mqttbytes::PacketType::PingReq | mqttbytes::PacketType::PingResp
                if header.frame_length() != 2 =>
            {
                return Err(malformed("MQTT", "nonempty ping packet"));
            }
            mqttbytes::PacketType::Disconnect if header.frame_length() == 2 => {
                let _ = self.bytes.split_to(2);
                return Ok(Some(Packet::Disconnect(v5::Disconnect::new())));
            }
            _ => {}
        }
        // MQTT 5 permits omitting Property Length on reason-only DISCONNECT
        // and QoS acknowledgements. mqttbytes 0.6 expects it unconditionally.
        // Supply the equivalent empty property set to its typed decoder.
        let reason_only = (self.bytes[0] == 0xe0 && self.bytes[1] == 1)
            || (matches!(self.bytes[0] >> 4, 4..=7) && self.bytes[1] == 3);
        if reason_only {
            let length = header.frame_length();
            let mut packet = BytesMut::from(&self.bytes[..length]);
            packet[1] += 1;
            packet.extend_from_slice(&[0]);
            let decoded = v5::read(&mut packet, MAX_FRAME_SIZE)
                .map_err(|_| malformed("MQTT", "invalid packet body"))?;
            let _ = self.bytes.split_to(length);
            return Ok(Some(decoded));
        }
        // Framing has already proved completeness. An insufficient body is
        // malformed, even if the decoder reports InsufficientBytes after
        // consuming that frame; it must never masquerade as pending input.
        v5::read(&mut self.bytes, MAX_FRAME_SIZE)
            .map(Some)
            .map_err(|_| malformed("MQTT", "invalid packet body"))
    }
}

pub(super) fn packet_context(packet: &Packet) -> (&'static str, Option<u16>) {
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

pub(super) fn encode_packet(packet: &Packet) -> Result<Vec<u8>, CentralError> {
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
        Packet::Disconnect(packet) => packet.write(&mut bytes),
        Packet::PingReq => {
            bytes.extend_from_slice(&[0xc0, 0]);
            Ok(2)
        }
        Packet::PingResp => {
            bytes.extend_from_slice(&[0xd0, 0]);
            Ok(2)
        }
        _ => return Err(CentralError::Protocol),
    };
    result.map_err(|_| CentralError::Protocol)?;
    Ok(bytes.to_vec())
}
