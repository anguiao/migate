use std::{error::Error, fmt};

use crate::{
    device::DeviceKey,
    storage::{CachedPhysicalDevice, CachedScope, XiaomiRecord},
    xiaomi::{
        capability::{CapabilityError, parse_device},
        cloud::{CloudClient, CloudError, OwnedHome, group_id},
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
pub fn select_local_home(
    snapshot: &DiscoverySnapshot,
    homes: &[OwnedHome],
    uid: &str,
) -> Result<OwnedHome, CatalogError> {
    if !valid_identifier(uid) {
        return Err(CatalogError::InvalidScope);
    }
    for home in homes {
        validate_home(home)?;
    }
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
    if !valid_identifier(&scope.uid)
        || !valid_identifier(&scope.home_id)
        || scope.group_id != group_id(&scope.uid, &scope.home_id)
    {
        return Err(CatalogError::InvalidScope);
    }
    let mut selected = None;
    for home in homes {
        validate_home(home)?;
        if home.uid == scope.uid && home.home_id == scope.home_id {
            if home.group_id != scope.group_id || selected.is_some() {
                return Err(CatalogError::InvalidScope);
            }
            selected = Some(home.clone());
        }
    }
    selected.ok_or(CatalogError::NoLocalHome)
}

/// Load one member of the selected home. Persistence and availability belong to source.
pub async fn load_device(
    cloud: &CloudClient,
    credentials: &XiaomiRecord,
    home: &OwnedHome,
    did: &str,
) -> Result<CachedPhysicalDevice, CatalogError> {
    validate_home(home)?;
    if home.uid != credentials.uid || !valid_identifier(did) {
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
    let instance = cloud
        .get_instance(&details.instance_type)
        .await
        .map_err(CatalogError::Cloud)?;
    let device =
        parse_device(DeviceKey::new("xiaomi", did), &instance).map_err(CatalogError::Capability)?;
    Ok(CachedPhysicalDevice {
        uid: home.uid.clone(),
        home_id: home.home_id.clone(),
        group_id: home.group_id.clone(),
        model: details.model,
        instance_type: details.instance_type,
        device,
    })
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

fn validate_home(home: &OwnedHome) -> Result<(), CatalogError> {
    if !valid_identifier(&home.uid)
        || !valid_identifier(&home.home_id)
        || home.group_id != group_id(&home.uid, &home.home_id)
        || home.dids.iter().any(|did| !valid_identifier(did))
    {
        return Err(CatalogError::InvalidScope);
    }
    Ok(())
}
