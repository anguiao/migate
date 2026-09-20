mod actuators;
mod assembly;
mod climate;
mod lighting;
mod scope;
mod sensors;
mod validation;

use super::*;
use crate::device::{
    Capability, DeviceCommand, FeatureRole, Percent, PresenceState, Property, PropertyValue,
    RgbColor, SensingModality,
};
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;

fn spec(device: &str, services: serde_json::Value) -> String {
    json!({
        "type": format!("urn:miot-spec-v2:device:{device}:0000:test:1"),
        "description": "Test device",
        "services": services,
    })
    .to_string()
}

fn service(iid: u32, kind: &str, properties: serde_json::Value) -> serde_json::Value {
    json!({
        "iid": iid,
        "type": format!("urn:miot-spec-v2:service:{kind}:0000:test:1"),
        "description": kind,
        "properties": properties,
        "actions": [],
        "events": [],
    })
}

fn property(iid: u32, kind: &str, format: &str, access: &[&str]) -> serde_json::Value {
    json!({
        "iid": iid,
        "type": format!("urn:miot-spec-v2:property:{kind}:0000:test:1"),
        "description": kind,
        "format": format,
        "access": access,
    })
}

fn public_spec(model: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/miot_specs")
            .join(format!("{model}.json")),
    )
    .unwrap()
}
