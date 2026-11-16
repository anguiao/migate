use super::super::{commands::SharedFanPower, state::Lifetime};
use crate::{
    device::{PhysicalDevice, Power},
    xiaomi::capability::NativeDescription,
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

/// One logical delivery target and its command coordination. Native
/// subscriptions survive replacement; power coordination belongs to this sink.
pub(in crate::xiaomi::adapter) struct LogicalSink {
    pub(in crate::xiaomi::adapter) physical: PhysicalDevice,
    pub(in crate::xiaomi::adapter) life: Lifetime,
    shared_fan_power: RefCell<BTreeMap<String, Rc<RefCell<SharedFanPower>>>>,
}

impl LogicalSink {
    pub(in crate::xiaomi::adapter) fn new(physical: PhysicalDevice) -> Self {
        Self {
            physical,
            life: Lifetime::default(),
            shared_fan_power: RefCell::default(),
        }
    }

    pub(super) fn initialize(&self, native: &NativeDescription) {
        for function in native.device().functions() {
            if function
                .air_conditioner()
                .is_some_and(|ac| ac.mcn04().is_some())
            {
                self.physical
                    .function(function.id())
                    .unwrap()
                    .require_air_conditioner_source_constraints();
            }
        }
    }

    pub(super) fn inherit_relays(&self, previous: &Self, native: &NativeDescription) {
        for function in native
            .device()
            .functions()
            .iter()
            .filter(|function| function.relay_mode().is_some())
        {
            let observed = previous
                .physical
                .function(function.id())
                .unwrap()
                .snapshot();
            let next = self.physical.function(function.id()).unwrap();
            if let Some(power) = observed.power().and_then(Power::value) {
                next.observe(power);
            }
            if let Some(mode) = observed.relay_mode() {
                next.observe_relay_mode(mode);
            }
        }
    }

    pub(in crate::xiaomi::adapter) fn shared_fan_power(
        &self,
        id: &str,
    ) -> Rc<RefCell<SharedFanPower>> {
        self.shared_fan_power
            .borrow_mut()
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    pub(in crate::xiaomi::adapter) fn require_fan_power_read(&self, id: &str) {
        if let Some(power) = self.shared_fan_power.borrow().get(id) {
            power.borrow_mut().require_read();
        }
    }

    pub(in crate::xiaomi::adapter) fn close(&self) {
        self.physical.close();
        self.life.close();
    }
}
