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
    CommittedConfiguration {
        scope: Some(scope),
        root_version: 1,
        devices,
        groups: Vec::new(),
    }
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
