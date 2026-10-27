use super::*;

pub(in crate::matter) fn basic_info(identity: &Identity) -> BasicInfoConfig<'_> {
    BasicInfoConfig {
        product_name: "MiGate",
        device_name: "MiGate",
        product_label: "MiGate",
        serial_no: &identity.bridge_id,
        unique_id: &identity.bridge_id,
        ..TEST_DEV_DET
    }
}
/// Restore BasicInfoSettings using upstream recovery and align it to the
/// committed root version, preserving other fields of a valid record.
/// A valid record newer than the committed root is rejected. This does not
/// write; the caller installs and persists the settings after model startup.
pub(in crate::matter) fn prepare_basic_info(
    kv: impl KvBlobStoreAccess,
    root_version: u32,
) -> Result<BasicInfoSettings, Error> {
    let mut persist = persist::Persist::new(kv);
    match persist.load_tlv::<BasicInfoSettings>(persist::BASIC_INFO_KEY)? {
        Some(mut settings) => {
            if settings.configuration_version > root_version {
                return Err(ErrorCode::InvalidData.into());
            }
            settings.configuration_version = root_version;
            Ok(settings)
        }
        None => {
            let mut settings = BasicInfoSettings::new();
            settings
                .node_label
                .push_str("MiGate")
                .map_err(|_| ErrorCode::ConstraintError)?;
            settings.configuration_version = root_version;
            Ok(settings)
        }
    }
}

/// Persist the prepared settings, propagating serialization and storage errors.
pub(in crate::matter) fn persist_basic_info(
    kv: impl KvBlobStoreAccess,
    settings: &BasicInfoSettings,
) -> Result<(), Error> {
    persist::Persist::strict(kv).store_tlv(persist::BASIC_INFO_KEY, settings)
}
