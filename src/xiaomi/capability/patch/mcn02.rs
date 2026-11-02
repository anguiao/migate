use super::{
    super::{ParsedDevice, PropertyBinding, native::LegacyProfile},
    BuiltinPatch,
};
use crate::xiaomi::legacy_miio::{Conversion, Field, Mapping};

const PATCH: BuiltinPatch = BuiltinPatch {
    id: "lumi.mcn02.legacy-miio",
    stage: "direct transport",
};

static MAPPING: Mapping = Mapping {
    fields: &[
        Field {
            binding: PropertyBinding { siid: 2, piid: 1 },
            name: "power",
            setter: "set_power",
            conversion: Conversion::OnOff,
        },
        Field {
            binding: PropertyBinding { siid: 2, piid: 2 },
            name: "mode",
            setter: "set_mode",
            conversion: Conversion::Enum(&["auto", "cool", "dry", "heat", "wind"]),
        },
        Field {
            binding: PropertyBinding { siid: 2, piid: 3 },
            name: "tar_temp",
            setter: "set_tar_temp",
            conversion: Conversion::Temperature { min: 16, max: 30 },
        },
        Field {
            binding: PropertyBinding { siid: 3, piid: 1 },
            name: "fan_level",
            setter: "set_fan_level",
            conversion: Conversion::Enum(&["auto_fan", "small_fan", "medium_fan", "large_fan"]),
        },
        Field {
            binding: PropertyBinding { siid: 3, piid: 2 },
            name: "ver_swing",
            setter: "set_ver_swing",
            conversion: Conversion::OnOff,
        },
    ],
};

pub(in crate::xiaomi::capability) fn profile(
    model: &str,
    device: &ParsedDevice,
) -> Option<LegacyProfile> {
    if model != "lumi.acpartner.mcn02" {
        return None;
    }
    let function = device.functions().iter().find(|function| {
        function
            .air_conditioner()
            .is_some_and(|ac| MAPPING.supports(&ac.bindings()))
    })?;
    Some(LegacyProfile::new(PATCH, function.id(), &MAPPING))
}
