//! MQTT protocol mechanisms shared by the gateway session and the cloud
//! notification driver. The mechanisms return protocol facts; authentication,
//! subscriptions and business payload handling stay with each driver.

mod frames;
mod handshake;
mod incoming;
mod keepalive;
mod send;

pub(super) use frames::{Frames, MAX_FRAME_SIZE, encode_packet, packet_context};
pub(super) use handshake::{HandshakeError, handshake};
pub(super) use incoming::{QoS2, valid_publish};
pub(super) use keepalive::Keepalive;
pub(super) use send::{SendFrame, SendStatus};

/// Invalid MQTT framing, header or handshake content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ProtocolError;

pub(super) fn invalid(reason: &'static str) -> ProtocolError {
    log::debug!("Invalid MQTT message: reason={reason}");
    ProtocolError
}

#[cfg(test)]
mod tests;
