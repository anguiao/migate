//! Shared configuration fixtures for module tests. Values use the pure
//! configuration model and the real storage boundary.

#![allow(dead_code)]

use crate::xiaomi::capability::NativeDescription;

use crate::{
    configuration::{
        self, CommittedConfiguration, CommittedDevice, CommittedFunction, GroupRelation, PlanInput,
        Scope, SourceDevice, SourceFacts,
    },
    device::{DeviceKey, PhysicalDeviceDescription},
    matter,
    storage::Store,
    xiaomi::capability::parse_device_for_model,
};

pub(crate) fn scope() -> Scope {
    Scope {
        uid: "account".into(),
        home_id: "home".into(),
    }
}

pub(crate) fn unique(id: &str) -> String {
    let value = id.bytes().fold(1_u128, |value, byte| {
        value.wrapping_mul(31).wrapping_add(byte as u128)
    });
    format!("{value:0>32x}")
}

/// One committed identity with the current functions of a description and no
/// native facts (a historical reservation).
pub(crate) fn identity(description: &PhysicalDeviceDescription, endpoint: u16) -> CommittedDevice {
    let functions = description
        .functions()
        .iter()
        .enumerate()
        .map(|(index, function)| CommittedFunction {
            id: function.id.clone(),
            endpoint: endpoint + 1 + index as u16,
        })
        .collect();
    CommittedDevice {
        key: description.key().clone(),
        unique_id: unique(&description.key().id),
        endpoint,
        label: description.label().to_owned(),
        configuration_version: 1,
        functions,
        native: None,
    }
}

pub(crate) fn configuration(scope: Scope, devices: Vec<CommittedDevice>) -> CommittedConfiguration {
    let descriptions = devices
        .iter()
        .filter_map(|device| device.native.as_ref().map(raw_for))
        .collect();
    CommittedConfiguration {
        scope: Some(scope),
        root_version: 1,
        descriptions,
        devices,
        groups: Vec::new(),
    }
}

/// Source fixtures for tests that start from a manually assembled effective model.
pub(crate) fn raw_for(native: &NativeDescription) -> crate::xiaomi::capability::RawDescription {
    let source = match native.model() {
        "cuco.plug.cp7pd" => include_str!("../../../tests/fixtures/miot/cuco-cp7pd.json"),
        "devcea.light.ls2307" => {
            include_str!("../../../tests/fixtures/miot/devcea-light-ls2307.json")
        }
        "dmaker.fan.p5c" => include_str!("../../../tests/fixtures/miot/dmaker-p5c.json"),
        "xiaomi.curtain.acn010" => include_str!("../../../tests/fixtures/miot/xiaomi-acn010.json"),
        "miaomiaoce.sensor_ht.t6" => {
            include_str!("../../../tests/fixtures/miot/miaomiaoce-t6.json")
        }
        "xiaomi.light.ceil04" => {
            include_str!("../../../tests/fixtures/miot/xiaomi-light-ceil04.json")
        }
        "xiaomi.light.bar2" => include_str!("../../../tests/fixtures/miot/xiaomi-light-bar2.json"),
        "yeelink.light.light3" => {
            include_str!("../../../tests/fixtures/miot/yeelink-light-light3.json")
        }
        "yeelink.light.ml9" => include_str!("../../../tests/fixtures/miot/yeelink-light-ml9.json"),
        "yeelink.light.spot2" => {
            include_str!("../../../tests/fixtures/miot/yeelink-light-spot2.json")
        }
        "lemesh.light.wy0c15" => {
            include_str!("../../../tests/fixtures/miot/lemesh-light-wy0c15.json")
        }
        _ => include_str!("../../../tests/fixtures/miot/outlet.json"),
    };
    let mut instance: serde_json::Value = serde_json::from_str(source).unwrap();
    instance["type"] = native.instance_type().into();
    if native.model() == "miaomiaoce.sensor_ht.t6" && native.device().battery().is_none() {
        instance["services"]
            .as_array_mut()
            .unwrap()
            .retain(|service| {
                !service["type"]
                    .as_str()
                    .unwrap()
                    .contains(":service:battery:")
            });
    }
    crate::xiaomi::capability::RawDescription::new(
        native.device().key().clone(),
        native.model().into(),
        native.instance_type().into(),
        instance,
        None,
    )
    .unwrap()
}

pub(crate) fn relation(
    group_did: &str,
    members: &[&str],
    is_physical: bool,
    active: bool,
) -> GroupRelation {
    GroupRelation {
        group_did: group_did.into(),
        record_did: format!("record-{group_did}"),
        model: "vendor.group".into(),
        instance_type: None,
        is_physical,
        active,
        error: None,
        members: members.iter().map(|member| (*member).to_owned()).collect(),
    }
}

pub(crate) fn save(store: &Store, configuration: &CommittedConfiguration) {
    store
        .devices()
        .commit_configuration(configuration, &[])
        .unwrap();
}

pub(crate) fn native(id: &str, model: &str, instance: serde_json::Value) -> NativeDescription {
    NativeDescription::new(
        model.into(),
        instance["type"].as_str().unwrap().into(),
        parse_device_for_model(DeviceKey::new("xiaomi", id), model, &instance).unwrap(),
    )
    .unwrap()
}

/// Plan and commit an empty-to-one configuration for real native facts.
pub(crate) fn planned(store: &Store, natives: Vec<NativeDescription>) -> CommittedConfiguration {
    planned_from(store, CommittedConfiguration::empty(), natives)
}

pub(crate) fn planned_from(
    store: &Store,
    current: CommittedConfiguration,
    natives: Vec<NativeDescription>,
) -> CommittedConfiguration {
    let facts = SourceFacts {
        scope: Some(scope()),
        descriptions: natives
            .iter()
            .map(|native| crate::configuration::SourceRaw {
                description: raw_for(native),
                fresh: true,
            })
            .collect(),
        devices: natives
            .into_iter()
            .map(|native| SourceDevice {
                key: native.device().key().clone(),
                name: Some(native.model().to_owned()),
                native,
                fresh: true,
            })
            .collect(),
        relations: Vec::new(),
    };
    let current_derived = current.derive();
    let new_identities = facts
        .devices
        .iter()
        .filter(|device| current.device(&device.key).is_none())
        .map(|device| (device.key.clone(), unique(&device.key.id)))
        .collect();
    let plan = configuration::plan(
        PlanInput {
            current: &current,
            current_derived: &current_derived,
            facts,
            revocations: Vec::new(),
            new_identities,
        },
        |functions, battery| matter::fixed_shape_with_battery(functions.iter().copied(), battery),
    )
    .unwrap();
    save(store, &plan.configuration);
    plan.configuration
}

pub(crate) fn device_key(id: &str) -> DeviceKey {
    DeviceKey::new("xiaomi", id)
}
