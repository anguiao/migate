use super::color::{kelvin_to_mired, rgb_to_xy};
use super::level::{LEVEL_CLUSTER, matter_level};
use super::power::ON_OFF_CLUSTER;
use super::{AdjustmentKind, LightingHandler, SceneConfirmation};
use crate::device::{
    Capability, CommandIntent, DeviceCommand, Property, PropertyState, PropertyValue, RgbColor,
};
use rs_matter::{
    dm::{
        Cluster, HandlerContext,
        clusters::app::color_control::{RgbGamma, SetDeviceColor},
        clusters::decl::{color_control, groups, level_control, on_off, scenes_management},
        clusters::scenes::{
            AttributeValuePairStruct, AttributeValuePairStructArrayBuilder, SceneClusterHandler,
        },
    },
    error::{Error, ErrorCode},
    tlv::{TLVArray, TLVBuilderParent},
    with,
};
use std::{collections::BTreeMap, num::NonZeroU8};

pub(in crate::matter) const GROUPS_CLUSTER: Cluster<'static> = groups::FULL_CLUSTER
    .with_features(groups::Feature::GROUP_NAMES.bits())
    .with_attrs(with!(required))
    .with_cmds(with!(all))
    .with_events(with!());

pub(in crate::matter) const SCENES_CLUSTER: Cluster<'static> = scenes_management::FULL_CLUSTER
    .with_features(scenes_management::Feature::empty().bits())
    .with_attrs(with!(required))
    .with_cmds(with!(all))
    .with_events(with!());

fn expected_command_value(command: DeviceCommand) -> Option<(Property, PropertyValue)> {
    match command {
        DeviceCommand::SetPower(value) => Some((Property::Power, PropertyValue::Power(value))),
        DeviceCommand::SetBrightness(value) => {
            Some((Property::Brightness, PropertyValue::Percent(value)))
        }
        DeviceCommand::SetColorTemperature(value) => Some((
            Property::ColorTemperature,
            PropertyValue::ColorTemperature(value),
        )),
        DeviceCommand::SetColor(value) => Some((Property::Color, PropertyValue::Color(value))),
        _ => None,
    }
}

pub(in crate::matter) struct SceneOnOff<'a>(pub(in crate::matter) &'a LightingHandler);

pub(in crate::matter) struct SceneLevel<'a>(pub(in crate::matter) &'a LightingHandler);

pub(in crate::matter) struct SceneColor<'a>(pub(in crate::matter) &'a LightingHandler);

impl SceneClusterHandler for SceneOnOff<'_> {
    const CLUSTER_ID: u32 = ON_OFF_CLUSTER.id;

    fn endpoint_id(&self) -> u16 {
        self.0.endpoint
    }

    fn is_scenable_attribute(attribute_id: u32) -> bool {
        attribute_id == on_off::AttributeId::OnOff as u32
    }

    fn capture<P: TLVBuilderParent>(
        &self,
        values: AttributeValuePairStructArrayBuilder<P>,
    ) -> Result<AttributeValuePairStructArrayBuilder<P>, Error> {
        values.push_u8(
            on_off::AttributeId::OnOff as _,
            self.0.power().ok_or(ErrorCode::Failure)? as u8,
        )
    }

    async fn apply<C: HandlerContext>(
        &self,
        _ctx: &C,
        values: &TLVArray<'_, AttributeValuePairStruct<'_>>,
        _transition_time_ms: u32,
    ) -> Result<(), Error> {
        for value in values.iter() {
            let value = value?;
            if value.attribute_id()? == on_off::AttributeId::OnOff as u32
                && let Some(power) = value.value_unsigned_8()?
            {
                let power = match power {
                    0 => false,
                    1 => true,
                    _ => return Err(ErrorCode::ConstraintError.into()),
                };
                return self
                    .0
                    .scene_command(vec![DeviceCommand::SetPower(power)])
                    .await;
            }
        }
        Ok(())
    }
}

impl SceneClusterHandler for SceneLevel<'_> {
    const CLUSTER_ID: u32 = LEVEL_CLUSTER.id;

    fn endpoint_id(&self) -> u16 {
        self.0.endpoint
    }

    fn is_scenable_attribute(attribute_id: u32) -> bool {
        attribute_id == level_control::AttributeId::CurrentLevel as u32
    }

    fn capture<P: TLVBuilderParent>(
        &self,
        values: AttributeValuePairStructArrayBuilder<P>,
    ) -> Result<AttributeValuePairStructArrayBuilder<P>, Error> {
        values.push_u8(
            level_control::AttributeId::CurrentLevel as _,
            self.0.current_level_value().ok_or(ErrorCode::Failure)?,
        )
    }

    async fn apply<C: HandlerContext>(
        &self,
        _ctx: &C,
        values: &TLVArray<'_, AttributeValuePairStruct<'_>>,
        transition_time_ms: u32,
    ) -> Result<(), Error> {
        for value in values.iter() {
            let value = value?;
            if value.attribute_id()? == level_control::AttributeId::CurrentLevel as u32
                && let Some(level) = value.value_unsigned_8()?
            {
                if level == u8::MAX {
                    return Err(ErrorCode::ConstraintError.into());
                }
                if transition_time_ms >= 100 {
                    let intent = self
                        .0
                        .scene_intent
                        .borrow()
                        .as_ref()
                        .cloned()
                        .ok_or(ErrorCode::Failure)?;
                    if let Some(scene) = self.0.scene_confirmation.borrow_mut().as_mut() {
                        for command in self.0.commands_for_level(level.max(1), false)? {
                            if let Some((property, value)) = expected_command_value(command) {
                                scene.expected.insert(property, value);
                            }
                        }
                    }
                    return self.0.add_scene_adjustment(
                        intent,
                        transition_time_ms / 100,
                        AdjustmentKind::Brightness {
                            start: self.0.current_level_value().ok_or(ErrorCode::Failure)?,
                            target: level.max(1),
                            with_on_off: false,
                        },
                    );
                }
                return self
                    .0
                    .scene_command(self.0.commands_for_level(level.max(1), false)?)
                    .await;
            }
        }
        Ok(())
    }
}

impl SceneClusterHandler for SceneColor<'_> {
    const CLUSTER_ID: u32 = color_control::FULL_CLUSTER.id;

    fn endpoint_id(&self) -> u16 {
        self.0.endpoint
    }

    fn is_scenable_attribute(attribute_id: u32) -> bool {
        matches!(
            attribute_id,
            id if id == color_control::AttributeId::CurrentX as u32
                || id == color_control::AttributeId::CurrentY as u32
                || id == color_control::AttributeId::ColorTemperatureMireds as u32
                || id == color_control::AttributeId::EnhancedColorMode as u32
        )
    }

    fn capture<P: TLVBuilderParent>(
        &self,
        values: AttributeValuePairStructArrayBuilder<P>,
    ) -> Result<AttributeValuePairStructArrayBuilder<P>, Error> {
        if self.0.capabilities.borrow().contains(&Capability::Color)
            && self.0.color_temperature_range().is_some()
        {
            return Err(ErrorCode::Failure.into());
        }
        if self.0.capabilities.borrow().contains(&Capability::Color) {
            let (x, y) = rgb_to_xy(self.0.color().ok_or(ErrorCode::Failure)?);
            let values = values.push_u16(color_control::AttributeId::CurrentX as _, x)?;
            let values = values.push_u16(color_control::AttributeId::CurrentY as _, y)?;
            return values.push_u8(
                color_control::AttributeId::EnhancedColorMode as _,
                color_control::EnhancedColorModeEnum::CurrentXAndCurrentY as u8,
            );
        }
        let values = values.push_u16(
            color_control::AttributeId::ColorTemperatureMireds as _,
            self.0.current_mireds().ok_or(ErrorCode::Failure)?,
        )?;
        values.push_u8(
            color_control::AttributeId::EnhancedColorMode as _,
            color_control::EnhancedColorModeEnum::ColorTemperatureMireds as u8,
        )
    }

    async fn apply<C: HandlerContext>(
        &self,
        _ctx: &C,
        values: &TLVArray<'_, AttributeValuePairStruct<'_>>,
        transition_time_ms: u32,
    ) -> Result<(), Error> {
        let mut x = None;
        let mut y = None;
        for value in values.iter() {
            let value = value?;
            if value.attribute_id()? == color_control::AttributeId::CurrentX as u32 {
                x = value.value_unsigned_16()?;
            }
            if value.attribute_id()? == color_control::AttributeId::CurrentY as u32 {
                y = value.value_unsigned_16()?;
            }
            if value.attribute_id()? == color_control::AttributeId::ColorTemperatureMireds as u32
                && let Some(mireds) = value.value_unsigned_16()?
            {
                if mireds == 0 {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let command =
                    DeviceCommand::SetColorTemperature(self.0.quantized_kelvin_for_mired(mireds)?);
                if transition_time_ms >= 100 {
                    if let Some(scene) = self.0.scene_confirmation.borrow_mut().as_mut()
                        && let Some((property, value)) = expected_command_value(command)
                    {
                        scene.expected.insert(property, value);
                    }
                    let intent = self
                        .0
                        .scene_intent
                        .borrow()
                        .as_ref()
                        .cloned()
                        .ok_or(ErrorCode::Failure)?;
                    return self.0.add_scene_adjustment(
                        intent,
                        transition_time_ms / 100,
                        AdjustmentKind::ColorTemperature {
                            start_mireds: self.0.current_mireds().ok_or(ErrorCode::Failure)?,
                            target_mireds: mireds,
                        },
                    );
                }
                return self.0.scene_command(vec![command]).await;
            }
        }
        if let (Some(x), Some(y)) = (x, y) {
            let (red, green, blue) = SetDeviceColor::Xy { x, y }.to_rgb(RgbGamma::SRgb);
            let command = DeviceCommand::SetColor(RgbColor { red, green, blue });
            if transition_time_ms >= 100 {
                if let Some(scene) = self.0.scene_confirmation.borrow_mut().as_mut()
                    && let Some((property, value)) = expected_command_value(command)
                {
                    scene.expected.insert(property, value);
                }
                let intent = self
                    .0
                    .scene_intent
                    .borrow()
                    .as_ref()
                    .cloned()
                    .ok_or(ErrorCode::Failure)?;
                let (start_x, start_y) = self.0.color().map(rgb_to_xy).ok_or(ErrorCode::Failure)?;
                return self.0.add_scene_adjustment(
                    intent,
                    transition_time_ms / 100,
                    AdjustmentKind::Xy {
                        start_x,
                        start_y,
                        target_x: x,
                        target_y: y,
                    },
                );
            }
            return self.0.scene_command(vec![command]).await;
        }
        Ok(())
    }
}

impl LightingHandler {
    pub(super) fn capture_global_scene(&self) -> Result<Vec<DeviceCommand>, Error> {
        let mut commands = vec![DeviceCommand::SetPower(
            self.power().ok_or(ErrorCode::Failure)?,
        )];
        if let Some(value) = self.brightness() {
            commands.push(DeviceCommand::SetBrightness(value));
        }
        if let Some(value) = self.color_temperature() {
            commands.push(DeviceCommand::SetColorTemperature(value));
        }
        if let Some(value) = self.color() {
            commands.push(DeviceCommand::SetColor(value));
        }
        Ok(commands)
    }

    pub(in crate::matter) fn begin_scene_recall(&self, fabric: NonZeroU8, group: u16, scene: u8) {
        self.adjustment.borrow_mut().take();
        let capabilities = self.capabilities.borrow();
        let properties = [
            capabilities
                .iter()
                .any(|capability| matches!(capability, Capability::Power { writable: true }))
                .then_some(Property::Power),
            capabilities
                .iter()
                .any(|capability| matches!(capability, Capability::Brightness(_)))
                .then_some(Property::Brightness),
            capabilities
                .contains(&Capability::Color)
                .then_some(Property::Color),
            capabilities
                .iter()
                .any(|capability| matches!(capability, Capability::ColorTemperature(_)))
                .then_some(Property::ColorTemperature),
        ]
        .into_iter()
        .flatten();
        *self.scene_intent.borrow_mut() =
            Some(self.service.begin_command_intent(&self.feature, properties));
        *self.scene_confirmation.borrow_mut() = Some(SceneConfirmation {
            fabric,
            group,
            scene,
            expected: BTreeMap::new(),
            confirmed: false,
        });
    }

    pub(in crate::matter) fn finish_scene_recall(&self, accepted: bool) -> i8 {
        self.scene_intent.borrow_mut().take();
        if !accepted {
            self.scene_confirmation.borrow_mut().take();
            -1
        } else {
            self.scene_state_changed()
        }
    }

    pub(super) async fn scene_command(&self, commands: Vec<DeviceCommand>) -> Result<(), Error> {
        let intent = self
            .scene_intent
            .borrow()
            .as_ref()
            .cloned()
            .ok_or(ErrorCode::Failure)?;
        self.intent_command(&intent, commands.clone()).await?;
        if let Some(scene) = self.scene_confirmation.borrow_mut().as_mut() {
            for command in commands {
                if let Some((property, value)) = expected_command_value(command) {
                    scene.expected.insert(property, value);
                }
            }
        }
        Ok(())
    }

    pub(in crate::matter) fn pending_scene(&self, fabric: NonZeroU8) -> Option<(u16, u8)> {
        self.scene_confirmation
            .borrow()
            .as_ref()
            .filter(|scene| scene.fabric == fabric && !scene.confirmed)
            .map(|scene| (scene.group, scene.scene))
    }

    pub(in crate::matter) fn scene_state_changed(&self) -> i8 {
        if self.scene_intent.borrow().is_some() {
            return 0;
        }
        let mut confirmation = self.scene_confirmation.borrow_mut();
        let Some(scene) = confirmation.as_mut() else {
            return -1;
        };
        let snapshot = self.service.snapshot(&self.feature);
        let matches = !scene.expected.is_empty()
            && scene.expected.iter().all(|(property, expected)| {
                snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.property(*property))
                    .is_some_and(|state| match state {
                        PropertyState::Current { value, .. } => {
                            self.scene_value_matches(*property, expected, value)
                        }
                        _ => false,
                    })
            });
        if matches {
            let changed = !scene.confirmed;
            scene.confirmed = true;
            i8::from(changed)
        } else if scene.confirmed {
            scene.confirmed = false;
            -1
        } else {
            0
        }
    }

    pub(super) fn scene_value_matches(
        &self,
        property: Property,
        expected: &PropertyValue,
        actual: &PropertyValue,
    ) -> bool {
        match (property, expected, actual) {
            (
                Property::Brightness,
                PropertyValue::Percent(expected),
                PropertyValue::Percent(actual),
            ) => matter_level(*expected) == matter_level(*actual),
            (
                Property::ColorTemperature,
                PropertyValue::ColorTemperature(expected),
                PropertyValue::ColorTemperature(actual),
            ) => kelvin_to_mired(*expected) == kelvin_to_mired(*actual),
            _ => expected == actual,
        }
    }

    pub(super) fn add_scene_adjustment(
        &self,
        intent: CommandIntent,
        duration_ds: u32,
        kind: AdjustmentKind,
    ) -> Result<(), Error> {
        if let Some(adjustment) = self.adjustment.borrow_mut().as_mut() {
            adjustment.kinds.push(kind);
            adjustment.last_steps = None;
            self.wake.notify(usize::MAX);
            Ok(())
        } else {
            self.start_adjustment(intent, duration_ds, kind)
        }
    }
}
