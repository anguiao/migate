use super::{AuthorizationAttempt, CLIENT_ID, CloudError, CloudErrorKind};
use crate::storage::DeviceToken;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use isahc::{
    AsyncReadResponseExt, HttpClient, Request,
    config::{Configurable as _, RedirectPolicy},
};
use serde_json::{Map, Value, json};
use sha1::{Digest as _, Sha1};
use std::{
    collections::{HashMap, HashSet},
    error::Error as StdError,
    fmt,
    time::Duration,
};
use url::Url;
use uuid::Uuid;

pub const CLOUD_BASE_URL: &str = "https://ha.api.io.mi.com";
pub const MIOT_SPEC_BASE_URL: &str = "https://miot-spec.org";

#[derive(Clone, Eq, PartialEq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenResponse")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HomePage {
    pub uid: Option<String>,
    pub dids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedCatalog {
    pub uid: String,
    pub homes: Vec<OwnedHome>,
    pub devices: Vec<CloudDevice>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedHome {
    pub id: String,
    pub name: String,
    pub group_id: String,
    pub dids: Vec<String>,
    pub rooms: Vec<OwnedRoom>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedRoom {
    pub id: String,
    pub name: String,
    pub dids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudDevice {
    pub did: String,
    pub uid: Option<String>,
    pub name: String,
    pub model: String,
    pub spec_type: Option<String>,
    pub pid: Option<i64>,
    pub token: Option<DeviceToken>,
    pub online: Option<bool>,
    pub local_ip: Option<String>,
    pub parent_id: Option<String>,
}

pub struct CloudClient {
    client: HttpClient,
    base_url: String,
    spec_base_url: String,
    pub(super) control_timeout: Duration,
}

impl CloudClient {
    pub fn new() -> Result<Self, CloudError> {
        Self::build(
            CLOUD_BASE_URL,
            Duration::from_secs(30),
            Duration::from_secs(5),
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: &str, timeout: Duration) -> Result<Self, CloudError> {
        Self::build(base_url, timeout, Duration::from_secs(5))
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_control_timeout(
        base_url: &str,
        timeout: Duration,
        control_timeout: Duration,
    ) -> Result<Self, CloudError> {
        Self::build(base_url, timeout, control_timeout)
    }

    fn build(
        base_url: &str,
        timeout: Duration,
        control_timeout: Duration,
    ) -> Result<Self, CloudError> {
        let parsed =
            Url::parse(base_url).map_err(|_| CloudError::input("configure cloud client"))?;
        if parsed.cannot_be_a_base() || parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(CloudError::input("configure cloud client"));
        }
        let client = HttpClient::builder()
            .timeout(timeout)
            .redirect_policy(RedirectPolicy::None)
            .build()
            .map_err(|_| CloudError::new("configure cloud client", CloudErrorKind::Network))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            spec_base_url: if base_url == CLOUD_BASE_URL {
                MIOT_SPEC_BASE_URL
            } else {
                base_url
            }
            .trim_end_matches('/')
            .to_owned(),
            control_timeout,
        })
    }

    pub async fn exchange_token(
        &self,
        attempt: &AuthorizationAttempt,
        code: &str,
    ) -> Result<TokenResponse, CloudError> {
        if code.is_empty() {
            return Err(CloudError::input("exchange authorization code"));
        }
        self.token_request(
            "exchange authorization code",
            json!({
                "client_id": CLIENT_ID,
                "redirect_uri": attempt.redirect_uri(),
                "code": code,
                "device_id": format!("ha.{}", attempt.oauth_client_uuid()),
            }),
        )
        .await
    }

    pub async fn refresh_token(
        &self,
        oauth_client_uuid: &str,
        redirect_uri: &str,
        refresh_token: &str,
    ) -> Result<TokenResponse, CloudError> {
        if Uuid::parse_str(oauth_client_uuid).is_err()
            || redirect_uri.is_empty()
            || refresh_token.is_empty()
        {
            return Err(CloudError::input("refresh access token"));
        }
        self.token_request(
            "refresh access token",
            json!({
                "client_id": CLIENT_ID,
                "redirect_uri": redirect_uri,
                "refresh_token": refresh_token,
            }),
        )
        .await
    }

    async fn token_request(
        &self,
        operation: &'static str,
        data: Value,
    ) -> Result<TokenResponse, CloudError> {
        let mut url = self.endpoint("/app/v2/ha/oauth/get_token", operation)?;
        url.query_pairs_mut().append_pair("data", &data.to_string());
        let request = Request::builder()
            .method("GET")
            .uri(url.as_str())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(String::new())
            .map_err(|_| CloudError::protocol(operation))?;
        let value = self.send(request, operation).await?;
        let result = result_object(&value, operation)
            .map_err(|error| error.with_oauth_code(oauth_error_code(&value)))?;
        let access_token = token_string(result, "access_token", operation)?;
        let refresh_token = token_string(result, "refresh_token", operation)?;
        let expires_in = result
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .ok_or_else(|| CloudError::protocol(operation))?;
        Ok(TokenResponse {
            access_token,
            refresh_token,
            expires_in,
        })
    }

    pub async fn get_home(&self, access_token: &str) -> Result<HomePage, CloudError> {
        let operation = "read Xiaomi homes";
        let value = self.protected_post(operation, "/app/v2/homeroom/gethome", access_token, json!({
            "limit": 150, "fetch_share": false, "fetch_share_dev": false, "plat_form": 0, "app_ver": 9
        })).await?;
        let result = result_object(&value, operation)?;
        let homes = result
            .get("homelist")
            .and_then(Value::as_array)
            .ok_or_else(|| CloudError::protocol(operation))?;
        let mut uid = None;
        let mut dids = Vec::new();
        let mut seen = HashSet::new();
        for home in homes {
            let home = home
                .as_object()
                .ok_or_else(|| CloudError::protocol(operation))?;
            if uid.is_none() {
                uid = parse_uid(home.get("uid"), operation)?;
            }
            collect_dids(home.get("dids"), &mut dids, &mut seen, operation)?;
            if let Some(rooms) = home.get("roomlist") {
                for room in rooms
                    .as_array()
                    .ok_or_else(|| CloudError::protocol(operation))?
                {
                    let room = room
                        .as_object()
                        .ok_or_else(|| CloudError::protocol(operation))?;
                    collect_dids(room.get("dids"), &mut dids, &mut seen, operation)?;
                }
            }
        }
        dids.truncate(150);
        Ok(HomePage { uid, dids })
    }

    pub async fn get_devices(&self, access_token: &str, dids: &[String]) -> Result<(), CloudError> {
        if dids.is_empty() {
            return Ok(());
        }
        let operation = "read Xiaomi devices";
        let value = self.protected_post(operation, "/app/v2/home/device_list_page", access_token, json!({
            "limit": 200, "get_split_device": true, "get_third_device": true, "dids": &dids[..dids.len().min(150)]
        })).await?;
        let result = result_object(&value, operation)?;
        let list = result
            .get("list")
            .and_then(Value::as_array)
            .ok_or_else(|| CloudError::protocol(operation))?;
        if list.iter().any(|device| !device.is_object()) {
            return Err(CloudError::protocol(operation));
        }
        Ok(())
    }

    pub async fn get_owned_catalog(&self, access_token: &str) -> Result<OwnedCatalog, CloudError> {
        self.get_owned_catalog_inner(access_token, None).await
    }

    pub async fn get_owned_catalog_for_uid(
        &self,
        access_token: &str,
        uid: &str,
    ) -> Result<OwnedCatalog, CloudError> {
        if uid.is_empty() {
            return Err(CloudError::input("read complete Xiaomi catalog"));
        }
        self.get_owned_catalog_inner(access_token, Some(uid)).await
    }

    async fn get_owned_catalog_inner(
        &self,
        access_token: &str,
        expected_uid: Option<&str>,
    ) -> Result<OwnedCatalog, CloudError> {
        let operation = "read complete Xiaomi catalog";
        let value = self.protected_post(operation, "/app/v2/homeroom/gethome", access_token, json!({
            "limit": 150, "fetch_share": false, "fetch_share_dev": false, "plat_form": 0, "app_ver": 9
        })).await?;
        let result = result_object(&value, operation)?;
        let homes_value = optional_array(result, "homelist", operation)?;
        let mut uid = expected_uid.map(str::to_owned);
        let mut homes = Vec::new();
        let mut indices = HashMap::new();
        for value in homes_value {
            let home = parse_home(value, operation)?;
            if uid.is_none() {
                uid = Some(home.0.clone());
            }
            if uid.as_deref() != Some(home.0.as_str()) {
                return Err(CloudError::protocol(operation));
            }
            indices.insert(home.1.id.clone(), homes.len());
            homes.push(home.1);
        }
        let uid = uid.ok_or_else(|| CloudError::protocol(operation))?;
        let mut cursor = optional_string(result, "max_id", operation)?;
        let mut has_more = optional_bool(result, "has_more", operation)?.unwrap_or(false);
        let mut seen_cursors = HashSet::new();
        while has_more {
            let current = cursor
                .clone()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| CloudError::protocol(operation))?;
            if !seen_cursors.insert(current.clone()) {
                return Err(CloudError::protocol(operation));
            }
            let value = self
                .protected_post(
                    operation,
                    "/app/v2/homeroom/get_dev_room_page",
                    access_token,
                    json!({ "start_id": current, "limit": 150 }),
                )
                .await?;
            let result = result_object(&value, operation)?;
            for value in optional_array(result, "info", operation)? {
                merge_home_page(value, &mut homes, &indices, operation)?;
            }
            has_more = optional_bool(result, "has_more", operation)?.unwrap_or(false);
            cursor = optional_string(result, "max_id", operation)?;
        }
        for home in &mut homes {
            home.group_id = home_group_id(&uid, &home.id);
            deduplicate(&mut home.dids);
            for room in &mut home.rooms {
                deduplicate(&mut room.dids);
            }
        }
        let mut requested = Vec::new();
        for home in &homes {
            requested.extend(home.dids.iter().cloned());
            for room in &home.rooms {
                requested.extend(room.dids.iter().cloned());
            }
        }
        deduplicate(&mut requested);
        let mut devices = Vec::new();
        for batch in requested.chunks(150) {
            let mut start_did: Option<String> = None;
            let mut seen = HashSet::new();
            loop {
                let mut body = json!({ "limit": 200, "get_split_device": true, "get_third_device": true, "dids": batch });
                if let Some(cursor) = &start_did {
                    body["start_did"] = Value::String(cursor.clone());
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
                for value in optional_array(result, "list", operation)? {
                    devices.push(parse_device(value, &uid, operation)?);
                }
                if !optional_bool(result, "has_more", operation)?.unwrap_or(false) {
                    break;
                }
                let next = result
                    .get("next_start_did")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| CloudError::protocol(operation))?
                    .to_owned();
                if !seen.insert(next.clone()) {
                    return Err(CloudError::protocol(operation));
                }
                start_did = Some(next);
            }
            let returned: HashSet<_> = devices.iter().map(|device| device.did.as_str()).collect();
            if batch.iter().any(|did| {
                !returned.iter().any(|returned| {
                    logical_device_did(returned) == logical_device_did(did.as_str())
                })
            }) {
                return Err(CloudError::protocol(operation));
            }
        }
        Ok(OwnedCatalog {
            uid,
            homes,
            devices,
        })
    }

    pub async fn get_spec_instance(&self, type_urn: &str) -> Result<String, CloudError> {
        let operation = "read MIoT spec";
        if !type_urn.starts_with("urn:miot-spec-v2:device:") {
            return Err(CloudError::input(operation));
        }
        let mut url = Url::parse(&format!("{}/miot-spec-v2/instance", self.spec_base_url))
            .map_err(|_| CloudError::protocol(operation))?;
        url.query_pairs_mut().append_pair("type", type_urn);
        let request = Request::builder()
            .method("GET")
            .uri(url.as_str())
            .body(String::new())
            .map_err(|_| CloudError::protocol(operation))?;
        let value = self.send(request, operation).await?;
        let object = value
            .as_object()
            .ok_or_else(|| CloudError::protocol(operation))?;
        if object.get("type").and_then(Value::as_str) != Some(type_urn)
            || !object.get("services").is_some_and(Value::is_array)
        {
            return Err(CloudError::protocol(operation));
        }
        serde_json::to_string(&value).map_err(|_| CloudError::protocol(operation))
    }

    pub async fn get_certificate(
        &self,
        access_token: &str,
        csr_pem: &str,
    ) -> Result<String, CloudError> {
        if csr_pem.is_empty() {
            return Err(CloudError::input("request gateway certificate"));
        }
        let operation = "request gateway certificate";
        let value = self
            .protected_post(
                operation,
                "/app/v2/ha/oauth/get_central_crt",
                access_token,
                json!({
                    "csr": STANDARD.encode(csr_pem.as_bytes())
                }),
            )
            .await?;
        nonempty_string(result_object(&value, operation)?, "cert", operation)
    }

    async fn protected_post(
        &self,
        operation: &'static str,
        path: &str,
        access_token: &str,
        body: Value,
    ) -> Result<Value, CloudError> {
        self.protected_post_with_timeout(operation, path, access_token, body, None)
            .await
    }

    pub(super) async fn protected_post_with_timeout(
        &self,
        operation: &'static str,
        path: &str,
        access_token: &str,
        body: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, CloudError> {
        if access_token.is_empty() {
            return Err(CloudError::input(operation));
        }
        let url = self.endpoint(path, operation)?;
        let mut builder = Request::builder()
            .method("POST")
            .uri(url.as_str())
            .header("Content-Type", "application/json")
            .header("X-Client-BizId", "haapi")
            .header("X-Client-AppId", CLIENT_ID.to_string())
            .header("Authorization", format!("Bearer{access_token}"));
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let request = builder
            .body(body.to_string())
            .map_err(|_| CloudError::protocol(operation))?;
        self.send(request, operation).await
    }

    fn endpoint(&self, path: &str, operation: &'static str) -> Result<Url, CloudError> {
        Url::parse(&format!("{}{path}", self.base_url)).map_err(|_| CloudError::protocol(operation))
    }

    async fn send(
        &self,
        request: Request<String>,
        operation: &'static str,
    ) -> Result<Value, CloudError> {
        let mut response = self.client.send_async(request).await.map_err(|error| {
            CloudError::new(
                operation,
                if error.is_timeout() {
                    CloudErrorKind::Timeout
                } else {
                    CloudErrorKind::Network
                },
            )
        })?;
        let status = response.status().as_u16();
        if status == 401 {
            return Err(CloudError::new(operation, CloudErrorKind::Unauthorized));
        }
        if status != 200 {
            return Err(CloudError::new(
                operation,
                CloudErrorKind::HttpStatus(status),
            ));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| CloudError::new(operation, response_read_error_kind(&error)))?;
        serde_json::from_slice(&body).map_err(|_| CloudError::protocol(operation))
    }
}

fn parse_home(value: &Value, operation: &'static str) -> Result<(String, OwnedHome), CloudError> {
    let object = value
        .as_object()
        .ok_or_else(|| CloudError::protocol(operation))?;
    let uid = scalar_string(object.get("uid"), operation)?;
    let id = scalar_string(object.get("id"), operation)?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| catalog_text_is_safe(value))
        .ok_or_else(|| CloudError::protocol(operation))?
        .to_owned();
    let dids = string_array(object.get("dids"), operation)?;
    let rooms = optional_array(object, "roomlist", operation)?
        .iter()
        .map(|value| {
            let room = value
                .as_object()
                .ok_or_else(|| CloudError::protocol(operation))?;
            Ok(OwnedRoom {
                id: scalar_string(room.get("id"), operation)?,
                name: catalog_optional_name(room.get("name"), operation)?,
                dids: string_array(room.get("dids"), operation)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        uid,
        OwnedHome {
            id,
            name,
            group_id: String::new(),
            dids,
            rooms,
        },
    ))
}
fn merge_home_page(
    value: &Value,
    homes: &mut [OwnedHome],
    indices: &HashMap<String, usize>,
    operation: &'static str,
) -> Result<(), CloudError> {
    let object = value
        .as_object()
        .ok_or_else(|| CloudError::protocol(operation))?;
    let id = scalar_string(object.get("id"), operation)?;
    let index = indices
        .get(&id)
        .copied()
        .ok_or_else(|| CloudError::protocol(operation))?;
    homes[index]
        .dids
        .extend(string_array(object.get("dids"), operation)?);
    for value in optional_array(object, "roomlist", operation)? {
        let room = value
            .as_object()
            .ok_or_else(|| CloudError::protocol(operation))?;
        let room_id = scalar_string(room.get("id"), operation)?;
        let dids = string_array(room.get("dids"), operation)?;
        if let Some(existing) = homes[index]
            .rooms
            .iter_mut()
            .find(|existing| existing.id == room_id)
        {
            existing.dids.extend(dids);
        } else {
            homes[index].rooms.push(OwnedRoom {
                id: room_id,
                name: catalog_optional_name(room.get("name"), operation)?,
                dids,
            });
        }
    }
    Ok(())
}
fn parse_device(
    value: &Value,
    uid: &str,
    operation: &'static str,
) -> Result<CloudDevice, CloudError> {
    let object = value
        .as_object()
        .ok_or_else(|| CloudError::protocol(operation))?;
    if object.get("owner").is_some_and(|owner| !owner.is_null()) {
        return Err(CloudError::protocol(operation));
    }
    let device_uid = parse_uid(object.get("uid"), operation)?;
    if device_uid
        .as_deref()
        .is_some_and(|device_uid| device_uid != uid)
    {
        return Err(CloudError::protocol(operation));
    }
    let token = optional_string(object, "token", operation)?
        .as_deref()
        .map(|token| parse_hex_token(token, operation).map(|value| DeviceToken(value.to_vec())))
        .transpose()?;
    Ok(CloudDevice {
        did: catalog_nonempty_string(object, "did", operation)?,
        uid: device_uid,
        name: catalog_nonempty_string(object, "name", operation)?,
        model: catalog_nonempty_string(object, "model", operation)?,
        spec_type: catalog_optional_string(object, "spec_type", operation)?,
        pid: optional_i64(object, "pid", operation)?,
        token,
        online: optional_bool(object, "isOnline", operation)?,
        local_ip: if object.contains_key("local_ip") {
            catalog_optional_string(object, "local_ip", operation)?
        } else {
            catalog_optional_string(object, "localIP", operation)?
        },
        parent_id: catalog_optional_string(object, "parent_id", operation)?,
    })
}
fn parse_hex_token(value: &str, operation: &'static str) -> Result<[u8; 16], CloudError> {
    if value.len() != 32 || !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err(CloudError::protocol(operation));
    }
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| CloudError::protocol(operation))?;
    }
    Ok(bytes)
}

fn logical_device_did(did: &str) -> &str {
    did.rsplit_once(".s")
        .and_then(|(parent, suffix)| {
            (!parent.is_empty()
                && !suffix.is_empty()
                && suffix.bytes().all(|byte| byte.is_ascii_digit()))
            .then_some(parent)
        })
        .unwrap_or(did)
}
fn optional_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<&'a [Value], CloudError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        _ => Err(CloudError::protocol(operation)),
    }
}
fn optional_bool(
    object: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<Option<bool>, CloudError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(CloudError::protocol(operation)),
    }
}
fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<Option<String>, CloudError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        _ => Err(CloudError::protocol(operation)),
    }
}
fn optional_i64(
    object: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<Option<i64>, CloudError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| CloudError::protocol(operation)),
        _ => Err(CloudError::protocol(operation)),
    }
}
fn string_array(value: Option<&Value>, operation: &'static str) -> Result<Vec<String>, CloudError> {
    match value {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| !value.is_empty() && catalog_text_is_safe(value))
                    .map(str::to_owned)
                    .ok_or_else(|| CloudError::protocol(operation))
            })
            .collect(),
        _ => Err(CloudError::protocol(operation)),
    }
}
fn scalar_string(value: Option<&Value>, operation: &'static str) -> Result<String, CloudError> {
    match value {
        Some(Value::String(value)) if !value.is_empty() && catalog_text_is_safe(value) => {
            Ok(value.clone())
        }
        Some(Value::Number(value)) if value.is_i64() || value.is_u64() => Ok(value.to_string()),
        _ => Err(CloudError::protocol(operation)),
    }
}
fn catalog_text_is_safe(value: &str) -> bool {
    !value.chars().any(char::is_control)
}

fn catalog_optional_name(
    value: Option<&Value>,
    operation: &'static str,
) -> Result<String, CloudError> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) if catalog_text_is_safe(value) => Ok(value.clone()),
        _ => Err(CloudError::protocol(operation)),
    }
}

fn catalog_nonempty_string(
    result: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<String, CloudError> {
    nonempty_string(result, key, operation).and_then(|value| {
        catalog_text_is_safe(&value)
            .then_some(value)
            .ok_or_else(|| CloudError::protocol(operation))
    })
}

fn catalog_optional_string(
    result: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<Option<String>, CloudError> {
    optional_string(result, key, operation)?.map_or(Ok(None), |value| {
        catalog_text_is_safe(&value)
            .then_some(Some(value))
            .ok_or_else(|| CloudError::protocol(operation))
    })
}
fn home_group_id(uid: &str, home_id: &str) -> String {
    let digest = Sha1::digest(format!("{uid}central_service{home_id}").as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn deduplicate(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn oauth_error_code(value: &Value) -> Option<i64> {
    let message = value.get("message")?.as_str()?;
    let details: Value = serde_json::from_str(message).ok()?;
    details.get("error")?.as_i64()
}

fn result_object<'a>(
    value: &'a Value,
    operation: &'static str,
) -> Result<&'a Map<String, Value>, CloudError> {
    let code = value
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| CloudError::protocol(operation))?;
    if code != 0 {
        return Err(CloudError::new(operation, CloudErrorKind::Business(code)));
    }
    value
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| CloudError::protocol(operation))
}

fn nonempty_string(
    result: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<String, CloudError> {
    result
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CloudError::protocol(operation))
}

fn token_string(
    result: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<String, CloudError> {
    let value = nonempty_string(result, key, operation)?;
    validate_token(&value, operation)?;
    Ok(value)
}

fn validate_token(value: &str, operation: &'static str) -> Result<(), CloudError> {
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(CloudError::protocol(operation));
    }
    Ok(())
}

fn parse_uid(value: Option<&Value>, operation: &'static str) -> Result<Option<String>, CloudError> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(Value::Number(value)) if value.is_i64() || value.is_u64() => {
            Ok(Some(value.to_string()))
        }
        Some(Value::Number(_)) => Err(CloudError::protocol(operation)),
        _ => Ok(None),
    }
}

fn response_read_error_kind(error: &std::io::Error) -> CloudErrorKind {
    if error.kind() == std::io::ErrorKind::TimedOut {
        return CloudErrorKind::Timeout;
    }
    let mut source = error.source();
    while let Some(current) = source {
        if current
            .downcast_ref::<isahc::Error>()
            .is_some_and(isahc::Error::is_timeout)
        {
            return CloudErrorKind::Timeout;
        }
        source = current.source();
    }
    CloudErrorKind::Network
}

fn collect_dids(
    value: Option<&Value>,
    dids: &mut Vec<String>,
    seen: &mut HashSet<String>,
    operation: &'static str,
) -> Result<(), CloudError> {
    let Some(value) = value else {
        return Ok(());
    };
    for did in value
        .as_array()
        .ok_or_else(|| CloudError::protocol(operation))?
    {
        let did = did
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CloudError::protocol(operation))?;
        if seen.insert(did.to_owned()) && dids.len() < 150 {
            dids.push(did.to_owned());
        }
    }
    Ok(())
}
