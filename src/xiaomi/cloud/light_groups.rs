//! Account-wide device directory with native light-group membership.
//!
//! The Xiaomi device-list endpoint is not scoped to a home and may return
//! records for any home owned by the account. This boundary therefore reads
//! the complete directory without a DID filter and leaves home selection to
//! the caller, which owns the authoritative home directory.

use super::{
    CloudClient, CloudError,
    catalog::Pagination,
    response::{description, null_list, valid_description},
};
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, Visitor},
};
use serde_json::{json, value::RawValue};
use std::{
    collections::{HashMap, HashSet},
    fmt,
};

/// A complete account device directory read.
///
/// An `Ok` value was read from every directory page without a request or
/// envelope failure, so it is an authoritative snapshot and absences may be
/// treated as deletions. Any page failure returns `Err` and no partial
/// snapshot; callers must keep the previous confirmed state instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudDeviceCatalog {
    pub devices: Vec<CloudCatalogDevice>,
}

/// One device record of the account directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudCatalogDevice {
    pub did: String,
    pub name: Option<String>,
    pub model: String,
    /// MIoT instance type, or `None` when the directory record has no usable
    /// description. Absence is a capability metadata gap and never invalidates
    /// the record's identity or group membership.
    pub instance_type: Option<String>,
    /// Online observation from the directory record. Missing or invalid
    /// values fail the whole snapshot instead of defaulting to online.
    pub online: bool,
    pub group: CloudGroupStatus,
}

/// Native group membership read status for one directory record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CloudGroupStatus {
    /// The record carries no `member_list` and is an ordinary device.
    NotGroup,
    /// The record carries a well-formed membership. An empty member list is a
    /// confirmed empty relation, not a read failure.
    Confirmed(CloudGroupMembership),
    /// The record carries a `member_list` that could not be trusted. Callers
    /// keep the last confirmed relation instead of treating it as empty.
    Failed,
}

/// A confirmed native group relation. Members are sorted by DID so directory
/// ordering cannot change the value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudGroupMembership {
    /// Authoritative group identity from `member_list.group_did`.
    pub group_did: String,
    /// Distinguishes physical from virtual groups; never inferred from model.
    pub is_physical: bool,
    pub members: Vec<String>,
}

#[derive(Deserialize)]
struct CatalogResponse {
    #[serde(deserialize_with = "null_list")]
    list: Vec<CatalogRecord>,
    has_more: Option<bool>,
    next_start_did: Option<String>,
}

#[derive(Deserialize)]
struct CatalogRecord {
    #[serde(deserialize_with = "description")]
    did: String,
    #[serde(deserialize_with = "description")]
    model: String,
    #[serde(default, deserialize_with = "optional_description")]
    spec_type: Option<String>,
    #[serde(rename = "isOnline")]
    online: bool,
    name: Option<String>,
    extra: Option<Box<RawValue>>,
}

/// The `extra` field is inspected while it is still raw so duplicate keys
/// inside its nested membership object cannot be silently collapsed.
#[derive(Deserialize)]
struct RawExtra {
    #[serde(default, deserialize_with = "present_raw")]
    member_list: Option<Box<RawValue>>,
}

fn present_raw<'de, D>(deserializer: D) -> Result<Option<Box<RawValue>>, D::Error>
where
    D: Deserializer<'de>,
{
    Box::<RawValue>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
struct RawMemberList {
    #[serde(deserialize_with = "description")]
    group_did: String,
    status: String,
    #[serde(deserialize_with = "member_ship")]
    member_ship: Vec<String>,
    is_physical: bool,
}

impl CloudClient {
    /// Read every page of the account device directory, including native
    /// group membership, without a DID filter.
    ///
    /// `Ok` is a complete authoritative snapshot: request, pagination, cursor
    /// and envelope failures return `Err` instead of a partial result. The
    /// caller selects the owned home by comparing record DIDs with its home
    /// directory; the response is never labeled as belonging to one home.
    pub async fn get_device_catalog(
        &self,
        access_token: &str,
    ) -> Result<CloudDeviceCatalog, CloudError> {
        let operation = "read Xiaomi device catalog";
        let mut devices = Vec::new();
        let mut record_dids = HashSet::new();
        let mut pages = Pagination::default();
        let mut cursor = None;
        loop {
            let mut body = json!({"limit":200,"get_split_device":true,"get_third_device":true});
            if let Some(start_did) = &cursor {
                body["start_did"] = json!(start_did);
            }
            let raw = self
                .protected_post_raw(
                    operation,
                    "/app/v2/home/device_list_page",
                    access_token,
                    body,
                )
                .await?;
            let response: CatalogResponse =
                serde_json::from_str(raw.get()).map_err(|_| CloudError::protocol(operation))?;
            for record in response.list {
                if !record_dids.insert(record.did.clone()) {
                    return Err(CloudError::protocol(operation));
                }
                let group = read_group(record.extra.as_deref());
                devices.push(CloudCatalogDevice {
                    did: record.did,
                    name: record.name.filter(|name| {
                        !name.trim().is_empty() && !name.chars().any(char::is_control)
                    }),
                    model: record.model,
                    instance_type: record.spec_type,
                    online: record.online,
                    group,
                });
            }
            cursor = pages.advance(
                response.has_more,
                response.next_start_did.filter(|did| valid_description(did)),
                operation,
            )?;
            if cursor.is_none() {
                break;
            }
        }
        resolve_group_identity_conflicts(&mut devices);
        devices.sort_by(|left, right| left.did.cmp(&right.did));
        Ok(CloudDeviceCatalog { devices })
    }
}

/// Treat an empty instance type as an absent description. Control characters
/// and non-string types remain boundary errors.
fn optional_description<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<String>::deserialize(deserializer)? {
        Some(value) if value.is_empty() => Ok(None),
        Some(value) if value.chars().any(char::is_control) => {
            Err(de::Error::custom("invalid description"))
        }
        value => Ok(value),
    }
}

fn read_group(extra: Option<&RawValue>) -> CloudGroupStatus {
    let Some(extra) = extra else {
        return CloudGroupStatus::NotGroup;
    };
    if !is_object(extra) {
        return CloudGroupStatus::Failed;
    }
    let Ok(extra) = serde_json::from_str::<RawExtra>(extra.get()) else {
        return CloudGroupStatus::Failed;
    };
    let Some(member_list) = extra.member_list else {
        return CloudGroupStatus::NotGroup;
    };
    read_member_list(&member_list)
}

fn read_member_list(member_list: &RawValue) -> CloudGroupStatus {
    if !is_object(member_list) {
        return CloudGroupStatus::Failed;
    }
    let Ok(member_list) = serde_json::from_str::<RawMemberList>(member_list.get()) else {
        return CloudGroupStatus::Failed;
    };
    if member_list.status != "1"
        || member_list
            .member_ship
            .iter()
            .any(|member| member == &member_list.group_did)
    {
        return CloudGroupStatus::Failed;
    }
    let mut members = member_list.member_ship;
    members.sort();
    CloudGroupStatus::Confirmed(CloudGroupMembership {
        group_did: member_list.group_did,
        is_physical: member_list.is_physical,
        members,
    })
}

/// A derived struct can also deserialize from a JSON sequence; group data is
/// only valid in a JSON object.
fn is_object(raw: &RawValue) -> bool {
    raw.get().trim_start().starts_with('{')
}

/// A group identity is authoritative only when exactly one record claims it.
/// Duplicate or conflicting claims become read failures for the involved
/// records so other groups keep their confirmations, while duplicate device
/// records are rejected by the caller before reaching this point.
fn resolve_group_identity_conflicts(devices: &mut [CloudCatalogDevice]) {
    let record_dids = devices
        .iter()
        .map(|device| device.did.clone())
        .collect::<HashSet<_>>();
    let mut group_counts = HashMap::new();
    for device in devices.iter() {
        if let CloudGroupStatus::Confirmed(group) = &device.group {
            *group_counts
                .entry(group.group_did.clone())
                .or_insert(0usize) += 1;
        }
    }
    for device in devices.iter_mut() {
        let failed = match &device.group {
            CloudGroupStatus::Confirmed(group) => {
                group_counts[group.group_did.as_str()] > 1
                    || (group.group_did != device.did
                        && record_dids.contains(group.group_did.as_str()))
            }
            _ => false,
        };
        if failed {
            device.group = CloudGroupStatus::Failed;
        }
    }
}

fn member_ship<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct MemberShip;

    impl<'de> Visitor<'de> for MemberShip {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map of member DIDs to status strings")
        }

        fn visit_map<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut seen = HashSet::new();
            let mut members = Vec::new();
            while let Some((did, status)) = entries.next_entry::<String, String>()? {
                if status != "1" || !valid_description(&did) || !seen.insert(did.clone()) {
                    return Err(de::Error::custom("invalid group member"));
                }
                members.push(did);
            }
            Ok(members)
        }
    }

    deserializer.deserialize_map(MemberShip)
}
