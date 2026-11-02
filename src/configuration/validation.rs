use super::CommittedConfiguration;
use crate::device::DeviceKey;
use std::collections::{BTreeMap, BTreeSet};

impl CommittedConfiguration {
    /// Structural and boundary validation, called when a record crosses the
    /// storage boundary. Internally planned values rely on their constructors
    /// and allocation invariants instead of repeating this check.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.root_version == 0 {
            return Err("Root configuration version is invalid");
        }
        if let Some(scope) = &self.scope {
            validate_text(&scope.uid)?;
            validate_text(&scope.home_id)?;
        } else if self.devices.iter().any(|device| device.native.is_some())
            || !self.descriptions.is_empty()
            || !self.groups.is_empty()
            || !self.temperature_bindings.is_empty()
        {
            return Err("Committed configuration has facts without a home scope");
        }
        let mut keys = BTreeSet::new();
        let mut unique_ids = BTreeSet::new();
        let mut endpoints: BTreeMap<u16, DeviceKey> = BTreeMap::new();
        for device in &self.devices {
            validate_text(&device.key.source)?;
            validate_text(&device.key.id)?;
            if !keys.insert(device.key.clone()) {
                return Err("Committed device identity is duplicated");
            }
            if !uuid::Uuid::parse_str(&device.unique_id)
                .is_ok_and(|uuid| uuid.simple().to_string() == device.unique_id)
                || !unique_ids.insert(device.unique_id.clone())
            {
                return Err("Committed device unique id is invalid or duplicated");
            }
            if !(2..=65534).contains(&device.endpoint) || device.configuration_version == 0 {
                return Err("Committed device identity is invalid");
            }
            if device.label.len() > 32 {
                return Err("Committed device label is invalid");
            }
            if device.functions.is_empty() {
                return Err("Committed device function set is empty");
            }
            let mut ids = BTreeSet::new();
            let mut own_endpoints = BTreeSet::new();
            for function in &device.functions {
                validate_text(&function.id)?;
                if !ids.insert(function.id.as_str())
                    || !(2..=65534).contains(&function.endpoint)
                    || !own_endpoints.insert(function.endpoint)
                {
                    return Err("Committed device function is invalid");
                }
                reserve_endpoint(&mut endpoints, function.endpoint, &device.key)?;
            }
            if own_endpoints.contains(&device.endpoint) {
                return Err("Committed device endpoint does not match its function topology");
            }
            reserve_endpoint(&mut endpoints, device.endpoint, &device.key)?;
            if let Some(native) = &device.native {
                if native.device().key() != &device.key {
                    return Err("Committed native facts belong to another device");
                }
                for function in native.device().functions() {
                    if !device
                        .functions
                        .iter()
                        .any(|entry| entry.id == function.id())
                    {
                        return Err("Committed native function has no reserved identity");
                    }
                }
            }
        }
        let mut sources = BTreeSet::new();
        for source in &self.descriptions {
            if !sources.insert(source.key.clone()) {
                return Err("Committed source description is duplicated");
            }
            let effective = self
                .device(&source.key)
                .and_then(|device| device.native.as_ref());
            match (effective, &source.diagnostic) {
                (Some(native), None)
                    if native.model() == source.model
                        && native.instance_type() == source.instance_type => {}
                (None, Some(_)) => {}
                _ => return Err("Committed source and effective description disagree"),
            }
        }
        if self
            .devices
            .iter()
            .any(|device| device.native.is_some() && !sources.contains(&device.key))
        {
            return Err("Committed source description is missing");
        }
        for (target, source) in &self.temperature_bindings {
            for (key, is_target) in [(target, true), (source, false)] {
                validate_text(&key.device.source)?;
                validate_text(&key.device.id)?;
                validate_text(&key.id)?;
                let Some(device) = self.device(&key.device) else {
                    return Err("Temperature binding device identity is missing");
                };
                if !device
                    .functions
                    .iter()
                    .any(|function| function.id == key.id)
                {
                    return Err("Temperature binding function identity is missing");
                }
                if let Some(native) = &device.native {
                    let function = native
                        .device()
                        .functions()
                        .iter()
                        .find(|function| function.id() == key.id);
                    let valid =
                        function.is_some_and(|function| match function.description().kind {
                            crate::device::FunctionKind::AirConditioner { .. } => is_target,
                            crate::device::FunctionKind::Temperature(_) => !is_target,
                            _ => false,
                        });
                    if !valid {
                        return Err("Temperature binding function type is invalid");
                    }
                }
            }
        }
        let mut groups = BTreeSet::new();
        for group in &self.groups {
            validate_text(&group.group_did)?;
            validate_text(&group.record_did)?;
            validate_text(&group.model)?;
            if let Some(instance_type) = &group.instance_type {
                validate_text(instance_type)?;
            }
            if let Some(error) = &group.error {
                validate_text(error)?;
            }
            if !groups.insert(group.group_did.as_str()) {
                return Err("Committed group relation is duplicated");
            }
            if let Some(source) = self
                .descriptions
                .iter()
                .find(|source| source.key.id == group.group_did)
            {
                if !source.is_group_instance() {
                    return Err("Committed group source is invalid");
                }
                if self
                    .device(&source.key)
                    .and_then(|device| device.native.as_ref())
                    .is_some_and(|native| native.device().unique_light().is_none())
                {
                    return Err("Committed group effective description is invalid");
                }
            }
            let mut members = BTreeSet::new();
            for member in &group.members {
                validate_text(member)?;
                if !members.insert(member.as_str()) {
                    return Err("Committed group relation member is duplicated");
                }
            }
        }
        Ok(())
    }
}

fn reserve_endpoint(
    endpoints: &mut BTreeMap<u16, DeviceKey>,
    endpoint: u16,
    key: &DeviceKey,
) -> Result<(), &'static str> {
    match endpoints.get(&endpoint) {
        Some(owner) if owner != key => Err("Committed endpoint is duplicated"),
        Some(_) => Ok(()),
        None => {
            endpoints.insert(endpoint, key.clone());
            Ok(())
        }
    }
}

fn validate_text(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.chars().any(char::is_control) {
        Err("Committed configuration metadata is invalid")
    } else {
        Ok(())
    }
}
