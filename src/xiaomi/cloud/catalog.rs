use super::{
    CloudClient, CloudError,
    client::{nonempty_string, result_object},
};
use isahc::Request;
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use std::collections::HashSet;
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

impl CloudClient {
    /// Read only owned homes. Other homes can be returned by the API, but no
    /// device details are requested while collecting this directory.
    pub async fn get_owned_homes(&self, access_token: &str) -> Result<OwnedHomes, CloudError> {
        let operation = "read owned Xiaomi homes";
        let value = self.protected_post(operation, "/app/v2/homeroom/gethome", access_token, json!({
            "limit":150, "fetch_share":false, "fetch_share_dev":false, "plat_form":0, "app_ver":9
        })).await?;
        let result = result_object(&value, operation)?;
        let records = required_list(result, "homelist", operation)?;
        let mut catalog = OwnedHomes {
            uid: None,
            homes: Vec::new(),
        };
        for record in records {
            let home = object(record, operation)?;
            let uid = identifier(home.get("uid"), operation)?;
            let home_id = identifier(home.get("id"), operation)?;
            if catalog.uid.as_ref().is_some_and(|known| *known != uid)
                || catalog.homes.iter().any(|known| known.home_id == home_id)
            {
                return Err(CloudError::protocol(operation));
            }
            catalog.uid = Some(uid.clone());
            let mut dids = Vec::new();
            merge_dids(home, &mut dids, operation)?;
            catalog.homes.push(OwnedHome {
                group_id: group_id(&uid, &home_id),
                uid,
                home_id,
                dids,
            });
        }
        let mut cursors = HashSet::new();
        let mut seen_homes: HashSet<String> = catalog
            .homes
            .iter()
            .map(|home| home.home_id.clone())
            .collect();
        let mut seen_members: HashSet<(String, String)> = catalog
            .homes
            .iter()
            .flat_map(|home| {
                home.dids
                    .iter()
                    .map(|did| (home.home_id.clone(), did.clone()))
            })
            .collect();
        let mut cursor = next_cursor(result, "max_id", records.len(), &mut cursors, operation)?;
        while let Some(start_id) = cursor {
            let value = self
                .protected_post(
                    operation,
                    "/app/v2/homeroom/get_dev_room_page",
                    access_token,
                    json!({"start_id":start_id,"limit":150}),
                )
                .await?;
            let result = result_object(&value, operation)?;
            let records = required_list(result, "info", operation)?;
            let mut progress = 0;
            for record in records {
                let record = object(record, operation)?;
                let id = identifier(record.get("id"), operation)?;
                progress += usize::from(seen_homes.insert(id.clone()));
                let mut dids = Vec::new();
                merge_dids(record, &mut dids, operation)?;
                for did in &dids {
                    progress += usize::from(seen_members.insert((id.clone(), did.clone())));
                }
                if let Some(home) = catalog.homes.iter_mut().find(|home| home.home_id == id) {
                    for did in dids {
                        if !home.dids.contains(&did) {
                            home.dids.push(did);
                        }
                    }
                }
            }
            cursor = next_cursor(result, "max_id", progress, &mut cursors, operation)?;
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
        let mut cursors = HashSet::new();
        let mut seen_devices = HashSet::new();
        let mut found = None;
        loop {
            let mut body =
                json!({"limit":200,"get_split_device":true,"get_third_device":true,"dids":[did]});
            if let Some(start_did) = &cursor {
                body["start_did"] = json!(start_did);
            }
            let value = self
                .protected_post(
                    operation,
                    "/app/v2/home/device_list_page",
                    access_token,
                    body,
                )
                .await?;
            let result = result_object(&value, operation)?;
            let records = required_list(result, "list", operation)?;
            let mut progress = 0;
            for record in records {
                let record = object(record, operation)?;
                let returned_did = description_string(record, "did", operation)?;
                progress += usize::from(seen_devices.insert(returned_did.clone()));
                if returned_did == did {
                    found = Some(CloudDevice {
                        did: returned_did,
                        model: description_string(record, "model", operation)?,
                        instance_type: description_string(record, "spec_type", operation)?,
                    });
                }
            }
            cursor = next_cursor(result, "next_start_did", progress, &mut cursors, operation)?;
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
        let value = self.send(request, operation).await?;
        if value.get("type").and_then(Value::as_str) != Some(instance_type) {
            return Err(CloudError::protocol(operation));
        }
        Ok(value)
    }
}

fn description_string(
    value: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<String, CloudError> {
    let value = nonempty_string(value, key, operation)?;
    if value.chars().any(char::is_control) {
        return Err(CloudError::protocol(operation));
    }
    Ok(value)
}

fn list<'a>(value: Option<&'a Value>, operation: &'static str) -> Result<&'a [Value], CloudError> {
    match value {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        _ => Err(CloudError::protocol(operation)),
    }
}

fn required_list<'a>(
    result: &'a Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<&'a [Value], CloudError> {
    let value = result
        .get(key)
        .ok_or_else(|| CloudError::protocol(operation))?;
    list(Some(value), operation)
}

fn object<'a>(
    value: &'a Value,
    operation: &'static str,
) -> Result<&'a Map<String, Value>, CloudError> {
    value
        .as_object()
        .ok_or_else(|| CloudError::protocol(operation))
}

fn identifier(value: Option<&Value>, operation: &'static str) -> Result<String, CloudError> {
    match value {
        Some(Value::String(value)) if !value.is_empty() && !value.chars().any(char::is_control) => {
            Ok(value.clone())
        }
        Some(Value::Number(value)) if value.is_i64() || value.is_u64() => Ok(value.to_string()),
        _ => Err(CloudError::protocol(operation)),
    }
}

fn merge_dids(
    home: &Map<String, Value>,
    dids: &mut Vec<String>,
    operation: &'static str,
) -> Result<(), CloudError> {
    let mut seen: HashSet<String> = dids.iter().cloned().collect();
    let rooms = list(home.get("roomlist"), operation)?;
    let mut lists = vec![list(home.get("dids"), operation)?];
    for room in rooms {
        lists.push(list(object(room, operation)?.get("dids"), operation)?);
    }
    for values in lists {
        for value in values {
            let did = value
                .as_str()
                .filter(|did| !did.is_empty() && !did.chars().any(char::is_control))
                .ok_or_else(|| CloudError::protocol(operation))?;
            if seen.insert(did.to_owned()) {
                dids.push(did.to_owned());
            }
        }
    }
    Ok(())
}

fn next_cursor(
    result: &Map<String, Value>,
    key: &str,
    records: usize,
    seen: &mut HashSet<String>,
    operation: &'static str,
) -> Result<Option<String>, CloudError> {
    let has_more = match result.get("has_more") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        _ => return Err(CloudError::protocol(operation)),
    };
    if !has_more {
        return Ok(None);
    }
    let cursor = nonempty_string(result, key, operation)?;
    if records == 0 || !seen.insert(cursor.clone()) {
        return Err(CloudError::protocol(operation));
    }
    Ok(Some(cursor))
}
