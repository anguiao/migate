use super::{
    Capability, Category, DeviceKey, FreshSnapshot, Function, FunctionKey, LightMode,
    LightSnapshot, NumericRange, Power, derive_group_capabilities,
};
use std::{
    cmp::Ordering,
    fmt,
    sync::{Arc, Mutex},
};

/// Capabilities and identity derived from the current member set. Nothing
/// here carries a Matter type or mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupDescription {
    pub key: DeviceKey,
    pub label: String,
    pub capabilities: Vec<Capability>,
}

impl GroupDescription {
    pub fn temperature_range(&self) -> Option<NumericRange> {
        self.capabilities
            .iter()
            .find_map(|capability| match capability {
                Capability::ColorTemperature(range) => Some(*range),
                _ => None,
            })
    }
}

/// State exposed by a light entry with its valid per-member observations.
/// Groups aggregate those observations; single lights retain their normal cache.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupSnapshot {
    pub power: Power,
    pub lighting: Option<LightSnapshot>,
    pub reachable: bool,
    pub representative: Option<FunctionKey>,
    pub members: Vec<FreshSnapshot>,
}

/// A rejected member binding. Rejection leaves the group unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupError {
    EmptyMembers,
    DuplicateMember(FunctionKey),
    UnsupportedMember(DeviceKey),
    SelfMember(FunctionKey),
}

impl fmt::Display for GroupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMembers => formatter.write_str("group has no members"),
            Self::DuplicateMember(key) => {
                write!(formatter, "group member {} is repeated", key.device.id)
            }
            Self::UnsupportedMember(key) => write!(
                formatter,
                "group member {} is not a supported single light",
                key.id
            ),
            Self::SelfMember(key) => write!(formatter, "group {} contains itself", key.device.id),
        }
    }
}

impl std::error::Error for GroupError {}

struct GroupState {
    label: String,
    capabilities: Vec<Capability>,
    members: Vec<Function>,
    last_brightness: Option<f64>,
}

struct Inner {
    key: DeviceKey,
    state: Mutex<GroupState>,
}

/// A light group bound to existing member functions by stable member keys.
/// The group never owns or copies a physical instance, command path or state
/// source of its own.
#[derive(Clone)]
pub struct Group(Arc<Inner>);

impl fmt::Debug for Group {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Group")
            .field("key", &self.0.key)
            .finish_non_exhaustive()
    }
}

impl Group {
    pub fn new(
        key: DeviceKey,
        label: impl Into<String>,
        members: Vec<Function>,
    ) -> Result<Self, GroupError> {
        let members = validate_members(&key, members)?;
        let capabilities = derive_group_capabilities(members.iter().map(Function::description));
        Ok(Self::from_validated(
            key,
            label.into(),
            members,
            capabilities,
        ))
    }

    /// Install the member binding and shape checked at the configuration boundary.
    pub(super) fn from_validated(
        key: DeviceKey,
        label: String,
        members: Vec<Function>,
        capabilities: Vec<Capability>,
    ) -> Self {
        Self(Arc::new(Inner {
            key,
            state: Mutex::new(GroupState {
                label,
                capabilities,
                members,
                last_brightness: None,
            }),
        }))
    }

    pub fn key(&self) -> &DeviceKey {
        &self.0.key
    }

    pub fn description(&self) -> GroupDescription {
        let state = self.0.state.lock().unwrap();
        GroupDescription {
            key: self.0.key.clone(),
            label: state.label.clone(),
            capabilities: state.capabilities.clone(),
        }
    }

    pub(super) fn has_capabilities(&self, capabilities: &[Capability]) -> bool {
        self.0.state.lock().unwrap().capabilities.as_slice() == capabilities
    }

    pub fn set_label(&self, label: impl Into<String>) {
        self.0.state.lock().unwrap().label = label.into();
    }

    /// Replace an already checked configuration without deriving its shape again.
    /// The stable aggregate handle keeps its process-local brightness memory.
    pub(super) fn replace_validated(
        &self,
        label: String,
        members: Vec<Function>,
        capabilities: Vec<Capability>,
    ) {
        let mut state = self.0.state.lock().unwrap();
        state.label = label;
        state.members = members;
        state.capabilities = capabilities;
    }

    pub fn functions(&self) -> Vec<Function> {
        self.0.state.lock().unwrap().members.clone()
    }

    pub fn keys(&self) -> Vec<FunctionKey> {
        self.0
            .state
            .lock()
            .unwrap()
            .members
            .iter()
            .map(|function| function.function_key().clone())
            .collect()
    }

    pub fn snapshot(&self) -> GroupSnapshot {
        let mut state = self.0.state.lock().unwrap();
        let capabilities = &state.capabilities;
        let members = state
            .members
            .iter()
            .map(Function::fresh_snapshot)
            .collect::<Vec<_>>();
        let reachable = members.iter().any(|member| member.reachable);
        let power = aggregate_power(&members);
        let all_power_known = members
            .iter()
            .all(|member| member.snapshot.power != Power::Unknown);
        let has_brightness = capabilities.contains(&Capability::Brightness);
        let has_rgb = capabilities.contains(&Capability::ColorRgb);
        let temperature_range = capabilities.iter().find_map(|capability| match capability {
            Capability::ColorTemperature(range) => Some(*range),
            _ => None,
        });
        let mut lighting =
            (has_brightness || temperature_range.is_some() || has_rgb).then_some(LightSnapshot {
                brightness_percent: None,
                temperature_kelvin: None,
                color: None,
                mode: None,
            });

        if has_brightness && power == Power::On && all_power_known {
            let mut sum = 0.0;
            let mut count = 0;
            let mut complete = true;
            for member in &members {
                if member.snapshot.power == Power::On {
                    let Some(value) = member
                        .snapshot
                        .lighting
                        .and_then(|lighting| lighting.brightness_percent)
                    else {
                        complete = false;
                        break;
                    };
                    sum += value;
                    count += 1;
                }
            }
            if complete && count > 0 {
                let average = sum / count as f64;
                state.last_brightness = Some(average);
                if let Some(lighting) = lighting.as_mut() {
                    lighting.brightness_percent = Some(average);
                }
            }
        } else if has_brightness
            && power == Power::Off
            && let Some(lighting) = lighting.as_mut()
        {
            lighting.brightness_percent = state.last_brightness;
        }

        let mut representative = None;
        // A group without any color capability must not project member color
        // fields that the intersection dropped.
        if temperature_range.is_some() || has_rgb {
            let candidate = members
                .iter()
                .filter(|member| member.reachable && active_color_known(member))
                .min_by_key(|member| {
                    if member.snapshot.power == Power::On {
                        0
                    } else {
                        1
                    }
                });
            if let Some(candidate) = candidate {
                representative = Some(candidate.key.clone());
                if let (Some(lighting), Some(observed)) =
                    (lighting.as_mut(), candidate.snapshot.lighting)
                {
                    lighting.mode = match observed.mode {
                        Some(LightMode::Temperature) if temperature_range.is_some() => {
                            observed.mode
                        }
                        Some(LightMode::Rgb) if has_rgb => observed.mode,
                        _ => None,
                    };
                    lighting.temperature_kelvin = observed.temperature_kelvin.filter(|value| {
                        temperature_range.is_some_and(|range| range.contains(*value))
                    });
                    lighting.color = observed.color.filter(|_| has_rgb);
                }
            }
        }

        GroupSnapshot {
            power,
            lighting,
            reachable,
            representative,
            members,
        }
    }
}

fn aggregate_power(members: &[FreshSnapshot]) -> Power {
    if members
        .iter()
        .any(|member| member.snapshot.power == Power::On)
    {
        Power::On
    } else if members
        .iter()
        .all(|member| member.snapshot.power == Power::Off)
    {
        Power::Off
    } else {
        Power::Unknown
    }
}

/// The representative needs a mode and an active value of that mode. A fixed
/// single-kind mode counts without a mode observation; a dual-color light only
/// becomes valid after an actual mode observation.
fn active_color_known(member: &FreshSnapshot) -> bool {
    member
        .snapshot
        .lighting
        .is_some_and(|lighting| match lighting.mode {
            Some(LightMode::Temperature) => lighting.temperature_kelvin.is_some(),
            Some(LightMode::Rgb) => lighting.color.is_some(),
            None => false,
        })
}

fn validate_members(
    group: &DeviceKey,
    members: Vec<Function>,
) -> Result<Vec<Function>, GroupError> {
    if members.is_empty() {
        return Err(GroupError::EmptyMembers);
    }
    let mut members = members;
    members.sort_by(|left, right| member_order(left.function_key(), right.function_key()));
    let mut previous: Option<&FunctionKey> = None;
    for member in &members {
        let key = member.function_key();
        if key.device == *group {
            return Err(GroupError::SelfMember(key.clone()));
        }
        if previous.is_some_and(|previous| previous == key) {
            return Err(GroupError::DuplicateMember(key.clone()));
        }
        let description = member.description();
        if description.id != "main"
            || description.category != Category::Light
            || !description.valid_capabilities()
        {
            return Err(GroupError::UnsupportedMember(key.device.clone()));
        }
        previous = Some(key);
    }
    Ok(members)
}

fn member_order(left: &FunctionKey, right: &FunctionKey) -> Ordering {
    (
        left.device.source.as_str(),
        left.device.id.as_str(),
        left.id.as_str(),
    )
        .cmp(&(
            right.device.source.as_str(),
            right.device.id.as_str(),
            right.id.as_str(),
        ))
}
