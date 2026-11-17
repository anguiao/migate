mod configuration;
mod configuration_air_conditioner;
mod configuration_bhf;
mod configuration_curtain;
mod configuration_fan;
mod configuration_heater;
mod configuration_sensors;
mod configuration_temperature_bindings;
pub(crate) mod fixtures;
pub(crate) use super::xiaomi::tests::{XiaomiTestFixture, credentials as xiaomi_credentials};
mod validity;

mod configuration_states;

mod lan;
mod xiaomi;
mod xiaomi_storage;
