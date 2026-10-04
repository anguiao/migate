use super::CentralError;
use bytes::BytesMut;
use mqttbytes::v5::{self, Packet};
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
    let mut value = value.as_bytes().to_vec();
    value.push(0);
    field(bytes, tag, &value);
}

pub fn decode_mips(mut bytes: &[u8]) -> Result<MipsMessage, CentralError> {
    let mut id = None;
    let mut payload = None;
    let mut seen = [false; 4];
    while !bytes.is_empty() {
        if bytes.len() < 5 {
            return Err(CentralError::Protocol);
        }
        let length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let tag = bytes[4] as usize;
        bytes = &bytes[5..];
        if length > bytes.len() {
            return Err(CentralError::Protocol);
        }
        let value = &bytes[..length];
        bytes = &bytes[length..];
        if tag >= seen.len() {
            continue;
        }
        if seen[tag] {
            return Err(CentralError::Protocol);
        }
        seen[tag] = true;
        if tag == 0 {
            id = Some(u32::from_le_bytes(
                value.try_into().map_err(|_| CentralError::Protocol)?,
            ));
        } else {
            // The TLV length bounds the string; gateways may omit its terminator.
            let value = value.strip_suffix(&[0]).unwrap_or(value);
            if value.contains(&0) {
                return Err(CentralError::Protocol);
            }
            let text = std::str::from_utf8(value).map_err(|_| CentralError::Protocol)?;
            if tag == 2 {
                payload = Some(serde_json::from_str(text).map_err(|_| CentralError::Protocol)?);
            }
        }
    }
    Ok(MipsMessage {
        id: id.ok_or(CentralError::Protocol)?,
        payload: payload.ok_or(CentralError::Protocol)?,
    })
}

#[derive(Default)]
pub struct Frames {
    bytes: BytesMut,
}

impl Frames {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), CentralError> {
        if self.bytes.len() + bytes.len() > MAX_FRAME_SIZE + 4096 {
            return Err(CentralError::Protocol);
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
                return Err(CentralError::Protocol);
            }
        }
        let header = match mqttbytes::check(self.bytes.iter(), MAX_FRAME_SIZE) {
            Ok(header) => header,
            Err(mqttbytes::Error::InsufficientBytes(_)) => return Ok(None),
            Err(_) => return Err(CentralError::Protocol),
        };
        match header.packet_type().map_err(|_| CentralError::Protocol)? {
            mqttbytes::PacketType::PingReq | mqttbytes::PacketType::PingResp
                if header.frame_length() != 2 =>
            {
                return Err(CentralError::Protocol);
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
            let decoded =
                v5::read(&mut packet, MAX_FRAME_SIZE).map_err(|_| CentralError::Protocol)?;
            let _ = self.bytes.split_to(length);
            return Ok(Some(decoded));
        }
        // Framing has already proved completeness. An insufficient body is
        // malformed, even if the decoder reports InsufficientBytes after
        // consuming that frame; it must never masquerade as pending input.
        v5::read(&mut self.bytes, MAX_FRAME_SIZE)
            .map(Some)
            .map_err(|_| CentralError::Protocol)
    }
}
