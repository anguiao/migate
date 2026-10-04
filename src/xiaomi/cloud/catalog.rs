use super::{
    CloudClient, CloudError,
    response::{Identifier, Members, Room, description, null_list, valid_description},
};
use isahc::Request;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedHome {
    pub uid: String,
    pub home_id: String,
    pub group_id: String,
    pub dids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedHomes {
    pub uid: Option<String>,
    pub homes: Vec<OwnedHome>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudDevice {
    pub did: String,
    pub model: String,
    pub instance_type: String,
}

pub fn group_id(uid: &str, home_id: &str) -> String {
    let digest = Sha1::digest(format!("{uid}central_service{home_id}"));
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Deserialize)]
struct HomesResponse {
    #[serde(deserialize_with = "null_list")]
    homelist: Vec<HomeRecord>,
    has_more: Option<bool>,
    max_id: Option<String>,
}

#[derive(Deserialize)]
struct HomeRecord {
    uid: Identifier,
    id: Identifier,
    #[serde(default, deserialize_with = "null_list")]
    dids: Vec<String>,
    roomlist: Vec<Room>,
}

#[derive(Deserialize)]
struct MembersResponse {
    #[serde(deserialize_with = "null_list")]
    info: Vec<MemberRecord>,
    has_more: Option<bool>,
    max_id: Option<String>,
}

#[derive(Deserialize)]
struct MemberRecord {
    id: Identifier,
    #[serde(flatten)]
    members: Members,
}

#[derive(Deserialize)]
struct DevicesResponse {
    #[serde(deserialize_with = "null_list")]
    list: Vec<DeviceRecord>,
    has_more: Option<bool>,
    next_start_did: Option<String>,
}

#[derive(Deserialize)]
struct DeviceRecord {
    #[serde(deserialize_with = "description")]
    did: String,
    #[serde(flatten)]
    details: Map<String, Value>,
}

#[derive(Deserialize)]
struct DeviceDescription {
    #[serde(deserialize_with = "description")]
    model: String,
    #[serde(deserialize_with = "description")]
    spec_type: String,
}

#[derive(Default)]
struct Pagination {
    cursors: HashSet<String>,
}

impl Pagination {
    fn advance(
        &mut self,
        has_more: Option<bool>,
        cursor: Option<String>,
        progress: usize,
        operation: &'static str,
    ) -> Result<Option<String>, CloudError> {
        if !has_more.unwrap_or(false) {
            return Ok(None);
        }
        let cursor = cursor
            .filter(|cursor| !cursor.is_empty())
            .ok_or_else(|| CloudError::protocol(operation))?;
        if progress == 0 || !self.cursors.insert(cursor.clone()) {
            return Err(CloudError::protocol(operation));
        }
        Ok(Some(cursor))
    }
}

impl CloudClient {
    /// Read only owned homes. Other homes can be returned by the API, but no
    /// device details are requested while collecting this directory.
    pub async fn get_owned_homes(&self, access_token: &str) -> Result<OwnedHomes, CloudError> {
        let operation = "read owned Xiaomi homes";
        let result: HomesResponse = self.protected_post(operation, "/app/v2/homeroom/gethome", access_token, json!({
            "limit":150, "fetch_share":false, "fetch_share_dev":false, "plat_form":0, "app_ver":9
        })).await?;
        let mut catalog = OwnedHomes {
            uid: None,
            homes: Vec::new(),
        };
        let mut home_indices = HashMap::new();
        let mut seen_members = HashSet::new();
        for record in result.homelist {
            let uid = record.uid.into_string();
            let home_id = record.id.into_string();
            if !valid_description(&uid)
                || !valid_description(&home_id)
                || catalog.uid.as_ref().is_some_and(|known| *known != uid)
                || home_indices
                    .insert(home_id.clone(), catalog.homes.len())
                    .is_some()
            {
                return Err(CloudError::protocol(operation));
            }
            catalog.uid = Some(uid.clone());
            let mut dids = Vec::new();
            for did in record
                .dids
                .into_iter()
                .chain(record.roomlist.into_iter().flat_map(|room| room.dids))
            {
                if !valid_description(&did) {
                    return Err(CloudError::protocol(operation));
                }
                if seen_members.insert((home_id.clone(), did.clone())) {
                    dids.push(did);
                }
            }
            catalog.homes.push(OwnedHome {
                group_id: group_id(&uid, &home_id),
                uid,
                home_id,
                dids,
            });
        }
        let mut pages = Pagination::default();
        let mut cursor = pages.advance(
            result.has_more,
            result.max_id,
            catalog.homes.len(),
            operation,
        )?;
        while let Some(start_id) = cursor {
            let result: MembersResponse = self
                .protected_post(
                    operation,
                    "/app/v2/homeroom/get_dev_room_page",
                    access_token,
                    json!({"start_id":start_id,"limit":150}),
                )
                .await?;
            let mut progress = 0;
            for record in result.info {
                let id = record.id.into_string();
                let index = home_indices
                    .get(&id)
                    .ok_or_else(|| CloudError::protocol(operation))?;
                for did in record.members.into_dids() {
                    if !valid_description(&did) {
                        return Err(CloudError::protocol(operation));
                    }
                    if seen_members.insert((id.clone(), did.clone())) {
                        progress += 1;
                        catalog.homes[*index].dids.push(did);
                    }
                }
            }
            cursor = pages.advance(result.has_more, result.max_id, progress, operation)?;
        }
        Ok(catalog)
    }

    /// The caller selects a home using local discovery before reaching this
    /// boundary. Restrict every page to the requested member DID.
    pub async fn get_device(
        &self,
        access_token: &str,
        home: &OwnedHome,
        did: &str,
    ) -> Result<Option<CloudDevice>, CloudError> {
        if !home.dids.iter().any(|member| member == did) {
            return Ok(None);
        }
        let operation = "read Xiaomi device description";
        let mut cursor = None;
        let mut pages = Pagination::default();
        let mut seen_devices = HashSet::new();
        let mut found = None;
        loop {
            let mut body =
                json!({"limit":200,"get_split_device":true,"get_third_device":true,"dids":[did]});
            if let Some(start_did) = &cursor {
                body["start_did"] = json!(start_did);
            }
            let result: DevicesResponse = self
                .protected_post(
                    operation,
                    "/app/v2/home/device_list_page",
                    access_token,
                    body,
                )
                .await?;
            let mut progress = 0;
            for record in result.list {
                progress += usize::from(seen_devices.insert(record.did.clone()));
                if record.did == did {
                    let details: DeviceDescription =
                        serde_json::from_value(Value::Object(record.details))
                            .map_err(|_| CloudError::protocol(operation))?;
                    found = Some(CloudDevice {
                        did: record.did,
                        model: details.model,
                        instance_type: details.spec_type,
                    });
                }
            }
            cursor = pages.advance(result.has_more, result.next_start_did, progress, operation)?;
            if cursor.is_none() {
                return Ok(found);
            }
        }
    }

    pub async fn get_instance(&self, instance_type: &str) -> Result<Value, CloudError> {
        let operation = "read MIoT instance description";
        if instance_type.is_empty() {
            return Err(CloudError::input(operation));
        }
        let mut url = Url::parse(&format!("{}/miot-spec-v2/instance", self.instance_base_url))
            .map_err(|_| CloudError::input(operation))?;
        url.query_pairs_mut().append_pair("type", instance_type);
        let request = Request::builder()
            .method("GET")
            .uri(url.as_str())
            .body(String::new())
            .map_err(|_| CloudError::protocol(operation))?;
        let value: Value = self.send(request, operation).await?;
        if value.get("type").and_then(Value::as_str) != Some(instance_type) {
            return Err(CloudError::protocol(operation));
        }
        Ok(value)
    }
}
