use std::{error::Error, fmt};

use crate::{
    device::DeviceKey,
    storage::{CachedPhysicalDevice, CachedScope, XiaomiRecord},
    xiaomi::{
        capability::{CapabilityError, parse_device_for_model},
        cloud::{CloudClient, CloudError, OwnedHome},
        discovery::DiscoverySnapshot,
    },
};

#[derive(Debug)]
pub enum CatalogError {
    NoLocalHome,
    InvalidScope,
    TargetMissing,
    Cloud(CloudError),
    Capability(CapabilityError),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLocalHome => {
                formatter.write_str("no owned Xiaomi home matches a local gateway")
            }
            Self::TargetMissing => {
                formatter.write_str("target device is missing from locally matched Xiaomi homes")
            }
            Self::InvalidScope => formatter.write_str("invalid Xiaomi account or home scope"),
            Self::Cloud(error) => error.fmt(formatter),
            Self::Capability(error) => error.fmt(formatter),
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cloud(error) => Some(error),
            Self::Capability(error) => Some(error),
            _ => None,
        }
    }
}

/// Select a local owned home before examining any device membership.
/// Home fields and uniqueness are validated by the cloud directory reader.
pub fn select_local_home(
    snapshot: &DiscoverySnapshot,
    homes: &[OwnedHome],
    uid: &str,
) -> Result<OwnedHome, CatalogError> {
    homes
        .iter()
        .filter(|home| {
            home.uid == uid
                && snapshot
                    .gateways
                    .iter()
                    .any(|gateway| gateway.group_id == home.group_id)
        })
        .min_by(|left, right| left.home_id.cmp(&right.home_id))
        .cloned()
        .ok_or(CatalogError::NoLocalHome)
}

/// A missing selected home is a scope failure, never an empty member list.
pub fn current_home(homes: &[OwnedHome], scope: &CachedScope) -> Result<OwnedHome, CatalogError> {
    homes
        .iter()
        .find(|home| home.uid == scope.uid && home.home_id == scope.home_id)
        .cloned()
        .ok_or(CatalogError::NoLocalHome)
}

/// Load a selected home's member description and optional cloud name.
/// Persistence and availability belong to the adapter.
pub async fn load_device(
    cloud: &CloudClient,
    credentials: &XiaomiRecord,
    home: &OwnedHome,
    did: &str,
) -> Result<(CachedPhysicalDevice, Option<String>), CatalogError> {
    if home.uid != credentials.uid {
        return Err(CatalogError::InvalidScope);
    }
    if !home.dids.iter().any(|member| member == did) {
        return Err(CatalogError::TargetMissing);
    }
    let details = cloud
        .get_device(&credentials.tokens.access_token, home, did)
        .await
        .map_err(CatalogError::Cloud)?
        .ok_or(CatalogError::TargetMissing)?;
    // A member list marks a Xiaomi group even when its DID or model would
    // otherwise pass ordinary light parsing; never read its MIoT description.
    if details.group.is_some() {
        return Err(CatalogError::Capability(
            CapabilityError::UnsupportedLightGroup,
        ));
    }
    let instance = cloud
        .get_instance(&details.instance_type)
        .await
        .map_err(CatalogError::Cloud)?;
    let device = parse_device_for_model(DeviceKey::new("xiaomi", did), &details.model, &instance)
        .map_err(CatalogError::Capability)?;
    Ok((
        CachedPhysicalDevice {
            uid: home.uid.clone(),
            home_id: home.home_id.clone(),
            group_id: home.group_id.clone(),
            model: details.model,
            instance_type: details.instance_type,
            device,
        },
        details.name,
    ))
}
