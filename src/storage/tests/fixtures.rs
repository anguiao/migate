use crate::{
    device::{FunctionKey, PhysicalDeviceDescription},
    storage::{
        CachedPhysicalDevice, CachedScope, PublicationCommit, StoredDevice, StoredFunction,
        import_label,
    },
};

pub(crate) fn scope() -> CachedScope {
    CachedScope {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: crate::xiaomi::cloud::group_id("account", "home"),
    }
}

pub(crate) fn record(description: &PhysicalDeviceDescription, endpoint: u16) -> StoredDevice {
    StoredDevice {
        key: description.key().clone(),
        unique_id: uuid::Uuid::new_v4().simple().to_string(),
        endpoint,
        label: import_label(description.label()),
        functions: description
            .functions()
            .iter()
            .enumerate()
            .map(|(index, function)| StoredFunction {
                key: FunctionKey::new(description.key().clone(), &function.id),
                endpoint: if description.functions().len() == 1 {
                    endpoint
                } else {
                    endpoint + index as u16 + 1
                },
                label: import_label(&function.label),
                kind: function.kind,
            })
            .collect(),
        published: true,
        configuration_version: 1,
    }
}

pub(crate) fn commit(
    scope: CachedScope,
    cached: Vec<CachedPhysicalDevice>,
    records: Vec<StoredDevice>,
) -> PublicationCommit {
    PublicationCommit {
        scope,
        cached,
        records,
        relations: Vec::new(),
        root_version: 1,
        clear_revocations: Vec::new(),
    }
}
