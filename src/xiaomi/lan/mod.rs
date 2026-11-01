//! Authenticated miIO datagrams for eligible IP devices.

mod client;
mod discovery;
mod wire;

pub(crate) use client::{Client, Failure, RequestError, SendStage};
pub(crate) use discovery::Discovery;

use std::fmt;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct Token([u8; 16]);

impl Token {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        if value.len() != 32 || !value.is_ascii() {
            return None;
        }
        let mut bytes = [0; 16];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            bytes[index] = (high * 16 + low) as u8;
        }
        Some(Self(bytes))
    }

    pub(crate) fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub(crate) fn bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Token([REDACTED])")
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::support as test_support;
