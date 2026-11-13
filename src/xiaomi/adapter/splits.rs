use crate::{configuration::SplitDevice, xiaomi::mijia::CloudDeviceCatalog};
use std::collections::BTreeSet;

/// Preserve only split records that occur in the selected home's directory.
/// The parent need not be listed there for an alias to be excluded.
pub(super) fn selected(
    catalog: &CloudDeviceCatalog,
    home: &BTreeSet<String>,
) -> Result<Vec<SplitDevice>, &'static str> {
    let mut channels = BTreeSet::new();
    let mut splits = Vec::new();
    for record in &catalog.devices {
        let Some(split) = &record.split else { continue };
        if !home.contains(&record.did) {
            continue;
        }
        if record.did == split.parent_did
            || !channels.insert((split.parent_did.as_str(), split.siid))
        {
            return Err("conflicting panel split records");
        }
        splits.push(SplitDevice {
            did: record.did.clone(),
            parent_did: split.parent_did.clone(),
            siid: split.siid,
            name: if home.contains(&split.parent_did) {
                record.name.clone()
            } else {
                None
            },
        });
    }
    Ok(splits)
}
