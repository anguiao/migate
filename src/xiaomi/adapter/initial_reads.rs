//! Initial source observations needed by the currently published functions.

use super::{
    state::{Context, Live},
    work::DeviceWork,
};
use crate::{
    configuration::CommittedConfiguration,
    device::{DeviceKey, FunctionDescription, FunctionKey},
    xiaomi::capability::{ParsedFunction, PropertyBinding},
};

pub(super) fn ready(context: &Context, configuration: &CommittedConfiguration) -> bool {
    let graph = context.graph.borrow();
    let live = context.live.borrow();

    for device in &graph.published {
        let Some(work) = current_work(&live, device) else {
            continue;
        };
        let group = graph.rules.get(&device.id);
        let virtual_group = group.is_some_and(|group| !group.is_physical);
        for function in graph.visible_functions(device) {
            if !virtual_group && !function_ready(work, &function.id) {
                return false;
            }
            if let Some(group) = group
                && (!group.is_physical || graph.served[device].repair.is_some())
            {
                for member in &group.members {
                    if !group_member_ready(&live, member, function, group.is_physical) {
                        return false;
                    }
                }
            }
            let key = FunctionKey::new(device.clone(), &function.id);
            if let Some(source) = configuration.temperature_bindings.get(&key)
                && let Some(source_work) = current_work(&live, &source.device)
                && !function_ready(source_work, &source.id)
            {
                return false;
            }
        }
        if work.physical().has_battery()
            && let Some(battery) = work.native.device().battery()
            && !work
                .refresh
                .borrow()
                .initial_complete("battery", battery.property())
        {
            return false;
        }
    }
    true
}

fn current_work<'a>(live: &'a Live, key: &DeviceKey) -> Option<&'a DeviceWork> {
    live.devices
        .get(&key.id)
        .and_then(|slot| slot.admitted.as_deref())
        .filter(|work| !work.life.is_closed() && work.key == *key)
}

fn native_function<'a>(work: &'a DeviceWork, id: &str) -> Option<&'a ParsedFunction> {
    work.native
        .device()
        .functions()
        .iter()
        .find(|function| function.id() == id)
}

fn function_ready(work: &DeviceWork, id: &str) -> bool {
    let Some(function) = native_function(work, id) else {
        return true;
    };
    let refresh = work.refresh.borrow();
    function
        .observation_bindings()
        .into_iter()
        .all(|binding| refresh.initial_complete(id, binding))
}

fn group_member_ready(
    live: &Live,
    key: &FunctionKey,
    published: &FunctionDescription,
    physical_group: bool,
) -> bool {
    let Some(work) = current_work(live, &key.device) else {
        return true;
    };
    let Some(function) = native_function(work, &key.id) else {
        return true;
    };
    let Some(light) = function.lighting() else {
        return true;
    };
    let refresh = work.refresh.borrow();
    let complete = |binding: Option<PropertyBinding>| {
        binding.is_none_or(|binding| refresh.initial_complete(&key.id, binding))
    };
    if !complete(function.power()) {
        return false;
    }
    if !physical_group
        && published.supports_brightness()
        && !complete(light.brightness().map(|value| value.property()))
    {
        return false;
    }
    if published.supports_color_temperature()
        && !complete(light.temperature().map(|value| value.property()))
    {
        return false;
    }
    if published.supports_rgb() && !complete(light.color().map(|value| value.property())) {
        return false;
    }
    if (published.supports_color_temperature() || published.supports_rgb())
        && !complete(light.mode().map(|value| value.property()))
    {
        return false;
    }
    true
}
