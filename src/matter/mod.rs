mod bridge;
mod bridged_info;
mod lighting;
mod model;
mod on_off;
mod pairing;
mod shape;
mod storage;

pub(crate) use bridge::Advertisement;
pub use bridge::Bridge;
pub use pairing::PairingEvent;

/// Shared canonical fixed-shape comparison consumed by publication next.
pub(crate) use shape::same_fixed_description;

#[cfg(test)]
pub(crate) mod tests;
