use super::CentralError;
use crate::xiaomi::mqtt;
use mqttbytes::v5::Packet;
use serde_json::Value;

pub(super) const MAX_FRAME_SIZE: usize = mqtt::MAX_FRAME_SIZE;

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
    inner: mqtt::Frames,
}

impl Frames {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), CentralError> {
        self.inner.push(bytes).map_err(|_| CentralError::Protocol)
    }

    pub fn next_packet(&mut self) -> Result<Option<Packet>, CentralError> {
        self.inner.next_packet().map_err(|_| CentralError::Protocol)
    }
}

pub(super) fn packet_context(packet: &Packet) -> (&'static str, Option<u16>) {
    mqtt::packet_context(packet)
}

pub(super) fn encode_packet(packet: &Packet) -> Result<Vec<u8>, CentralError> {
    mqtt::encode_packet(packet).map_err(|_| CentralError::Protocol)
}
