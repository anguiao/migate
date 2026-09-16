use std::{error::Error as StdError, fmt};

const TYPE_MID: u8 = 0;
const TYPE_RETURN_TOPIC: u8 = 1;
const TYPE_PAYLOAD: u8 = 2;
const TYPE_FROM: u8 = 3;
const MAX_MIPS_SIZE: usize = 256 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MipsEnvelope {
    pub mid: u32,
    pub return_topic: Option<String>,
    pub payload: String,
    pub from: Option<String>,
}

impl MipsEnvelope {
    pub fn request(mid: u32, return_topic: &str, payload: &str) -> Result<Self, MipsError> {
        if mid == 0 || !valid_string(return_topic) || payload.contains('\0') {
            return Err(MipsError);
        }
        Ok(Self {
            mid,
            return_topic: Some(return_topic.to_owned()),
            payload: payload.to_owned(),
            from: Some("local".to_owned()),
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, MipsError> {
        if self.payload.contains('\0') {
            return Err(MipsError);
        }
        let mut bytes = Vec::new();
        push_field(&mut bytes, TYPE_MID, &self.mid.to_le_bytes())?;
        if let Some(value) = &self.return_topic {
            push_string(&mut bytes, TYPE_RETURN_TOPIC, value)?;
        }
        push_string(&mut bytes, TYPE_PAYLOAD, &self.payload)?;
        if let Some(value) = &self.from {
            push_string(&mut bytes, TYPE_FROM, value)?;
        }
        if bytes.len() > MAX_MIPS_SIZE {
            return Err(MipsError);
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MipsError> {
        if bytes.len() > MAX_MIPS_SIZE {
            return Err(MipsError);
        }
        let mut cursor = 0;
        let mut mid = None;
        let mut return_topic = None;
        let mut payload = None;
        let mut from = None;
        while cursor < bytes.len() {
            if bytes.len() - cursor < 5 {
                return Err(MipsError);
            }
            let length = u32::from_le_bytes(
                bytes[cursor..cursor + 4]
                    .try_into()
                    .map_err(|_| MipsError)?,
            ) as usize;
            let field_type = bytes[cursor + 4];
            cursor += 5;
            let end = cursor.checked_add(length).ok_or(MipsError)?;
            let value = bytes.get(cursor..end).ok_or(MipsError)?;
            cursor = end;
            match field_type {
                TYPE_MID => {
                    if mid.is_some() || value.len() != 4 {
                        return Err(MipsError);
                    }
                    mid = Some(u32::from_le_bytes(value.try_into().map_err(|_| MipsError)?));
                }
                TYPE_RETURN_TOPIC => parse_string_field(value, &mut return_topic)?,
                TYPE_PAYLOAD => parse_string_field(value, &mut payload)?,
                TYPE_FROM => parse_string_field(value, &mut from)?,
                _ => {}
            }
        }
        let mid = mid.ok_or(MipsError)?;
        let payload = payload.ok_or(MipsError)?;
        Ok(Self {
            mid,
            return_topic,
            payload,
            from,
        })
    }
}

fn push_string(bytes: &mut Vec<u8>, field_type: u8, value: &str) -> Result<(), MipsError> {
    if !valid_string(value) {
        return Err(MipsError);
    }
    let mut encoded = value.as_bytes().to_vec();
    encoded.push(0);
    push_field(bytes, field_type, &encoded)
}

fn push_field(bytes: &mut Vec<u8>, field_type: u8, value: &[u8]) -> Result<(), MipsError> {
    let length = u32::try_from(value.len()).map_err(|_| MipsError)?;
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.push(field_type);
    bytes.extend_from_slice(value);
    Ok(())
}

fn parse_string_field(value: &[u8], target: &mut Option<String>) -> Result<(), MipsError> {
    if target.is_some() || value.last() != Some(&0) || value[..value.len() - 1].contains(&0) {
        return Err(MipsError);
    }
    let decoded = std::str::from_utf8(&value[..value.len() - 1]).map_err(|_| MipsError)?;
    if decoded.is_empty() {
        return Err(MipsError);
    }
    *target = Some(decoded.to_owned());
    Ok(())
}

fn valid_string(value: &str) -> bool {
    !value.is_empty() && !value.contains('\0')
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MipsError;

impl fmt::Display for MipsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Invalid MIPS envelope")
    }
}

impl StdError for MipsError {}
