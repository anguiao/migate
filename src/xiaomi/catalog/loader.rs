use std::{error::Error, fmt};

use crate::{
    device::DeviceKey,
    storage::{CachedDevice, XiaomiRecord},
    xiaomi::{
        capability::{CapabilityError, parse_outlet},
        cloud::{CloudClient, CloudError, OwnedHome},
        discovery::DiscoverySnapshot,
    },
};

#[derive(Debug)]
pub enum CatalogError {
    NoLocalHome,
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

/// Load only a current local home's member device using already processed
/// credentials. Persistence belongs to successful local device preparation.
pub async fn load_scoped(
    snapshot: &DiscoverySnapshot,
    homes: &[OwnedHome],
    credentials: &XiaomiRecord,
    key: &DeviceKey,
    cloud: &CloudClient,
) -> Result<CachedDevice, CatalogError> {
    let local_homes = homes
        .iter()
        .filter(|home| {
            home.uid == credentials.uid
                && snapshot
                    .gateways
                    .iter()
                    .any(|gateway| gateway.group_id == home.group_id)
        })
        .collect::<Vec<_>>();
    if local_homes.is_empty() {
        return Err(CatalogError::NoLocalHome);
    }
    let home = local_homes
        .into_iter()
        .filter(|home| home.dids.contains(&key.id))
        .min_by(|left, right| left.home_id.cmp(&right.home_id))
        .ok_or(CatalogError::TargetMissing)?;
    let device = cloud
        .get_device(&credentials.tokens.access_token, home, &key.id)
        .await
        .map_err(CatalogError::Cloud)?
        .ok_or(CatalogError::TargetMissing)?;
    let instance = cloud
        .get_instance(&device.instance_type)
        .await
        .map_err(CatalogError::Cloud)?;
    let outlet = parse_outlet(key.clone(), &instance).map_err(CatalogError::Capability)?;
    Ok(CachedDevice {
        uid: credentials.uid.clone(),
        home_id: home.home_id.clone(),
        group_id: home.group_id.clone(),
        model: device.model,
        instance_type: device.instance_type,
        outlet,
    })
}
