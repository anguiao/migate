use super::*;

#[derive(Default)]
pub(in super::super) struct CloudState {
    pub(in super::super) members: Vec<String>,
    pub(in super::super) names: BTreeMap<String, String>,
    pub(in super::super) models: BTreeMap<String, String>,
    pub(in super::super) status: u16,
    pub(in super::super) home_delay: Duration,
    pub(in super::super) home_gate: Option<flume::Receiver<()>>,
    pub(in super::super) home_status: u16,
    pub(in super::super) ha_homes: usize,
    pub(in super::super) cert_sign_csr: bool,
    pub(in super::super) home_bad_signature: bool,
    pub(in super::super) home_more: bool,
    pub(in super::super) member_pages: usize,
    pub(in super::super) foreign: bool,
    pub(in super::super) missing_home: bool,
    pub(in super::super) other_home: bool,
    pub(in super::super) partial: bool,
    pub(in super::super) unsupported: BTreeSet<String>,
    pub(in super::super) detail_status: u16,
    pub(in super::super) detail_delay: Duration,
    pub(in super::super) detail_gate: Option<flume::Receiver<()>>,
    pub(in super::super) details: usize,
    pub(in super::super) detail_done: usize,
    pub(in super::super) cert_status: u16,
    pub(in super::super) cert_gate: Option<flume::Receiver<()>>,
    pub(in super::super) cert_pem: Option<String>,
    pub(in super::super) certs: usize,
    pub(in super::super) token_status: u16,
    pub(in super::super) token_access: Option<String>,
    pub(in super::super) token_delay: Duration,
    pub(in super::super) token_gate: Option<flume::Receiver<()>>,
    pub(in super::super) tokens: usize,
    pub(in super::super) token_done: usize,
    pub(in super::super) token_lifetime: u64,
    pub(in super::super) homes: usize,
    pub(in super::super) home_requests: Vec<(String, String)>,
    pub(in super::super) restores: usize,
    pub(in super::super) restore_status: u16,
    pub(in super::super) restore_gate: Option<flume::Receiver<()>>,
    pub(in super::super) sts_requests: usize,
    pub(in super::super) online: BTreeSet<String>,
    pub(in super::super) infos: usize,
    pub(in super::super) info_status: u16,
    pub(in super::super) info_gate: Option<flume::Receiver<()>>,
    pub(in super::super) info_partial: bool,
    pub(in super::super) property_status: u16,
    pub(in super::super) property_code: i64,
    pub(in super::super) property_gate: Option<flume::Receiver<()>>,
    pub(in super::super) property_writes: Vec<(String, u32, u32, bool, String)>,
    pub(in super::super) property_reads: Vec<(String, u32, u32)>,
    pub(in super::super) read_batches: Vec<Vec<(String, u32, u32)>>,
    pub(in super::super) rpc_reads: Vec<(String, String, Value)>,
    pub(in super::super) rpc_results: BTreeMap<String, Value>,
    pub(in super::super) rpc_status: u16,
    pub(in super::super) rpc_gate: Option<flume::Receiver<()>>,
    pub(in super::super) action_requests: Vec<(String, u32, u32, Value)>,
    pub(in super::super) action_code: i64,
    pub(in super::super) action_status: u16,
    pub(in super::super) action_gate: Option<flume::Receiver<()>>,
    pub(in super::super) read_results: Option<Vec<Value>>,
    pub(in super::super) raw_values: BTreeMap<(String, u32, u32), Value>,
    pub(in super::super) update_times: BTreeMap<(String, u32, u32), i64>,
    pub(in super::super) property_codes: BTreeMap<(String, u32, u32), i64>,
    pub(in super::super) read_codes: BTreeMap<(String, u32, u32), i64>,
    pub(in super::super) numeric_writes: Vec<(Vec<Value>, String)>,
    pub(in super::super) read_values: BTreeMap<(String, u32, u32), bool>,
    pub(in super::super) read_gates: BTreeMap<(String, u32, u32), flume::Receiver<()>>,
    pub(in super::super) read_status: u16,
    /// Full unfiltered account-catalog records appended after the ordinary
    /// home members. Tests inject real `extra.member_list` shapes here.
    pub(in super::super) catalog_groups: Vec<Value>,
    /// Override account-directory records without changing per-device details.
    pub(in super::super) catalog_overrides: BTreeMap<String, Value>,
    pub(in super::super) detail_requests: Vec<Vec<String>>,
    pub(in super::super) catalog_status: u16,
    pub(in super::super) catalog_instances: BTreeMap<String, Value>,
    /// Per-DID cloud URN overrides. Unset DIDs keep their fixture value.
    pub(in super::super) instance_types: BTreeMap<String, String>,
    pub(in super::super) instance_gates: BTreeMap<String, flume::Receiver<()>>,
    pub(in super::super) instance_reads: BTreeMap<String, usize>,
    pub(in super::super) instance_status: BTreeMap<String, u16>,
    pub(in super::super) catalogs: usize,
    pub(in super::super) catalog_partial: bool,
    /// Per-DID gates holding that device's property write response.
    pub(in super::super) write_gates: BTreeMap<String, flume::Receiver<()>>,
}
pub(in super::super) struct Rig {
    pub(in super::super) uid: String,
    pub(in super::super) directory: Rc<tempfile::TempDir>,
    pub(in super::super) source: XiaomiAdapter,
    pub(in super::super) network: Rc<TestNetwork>,
    pub(in super::super) ready: TopologyReady,
    pub(in super::super) store: Store,
    pub(in super::super) discovery: Discovery,
    pub(in super::super) driver: crate::xiaomi::discovery::DiscoveryDriver,
    pub(in super::super) events: flume::Sender<mdns_sd::ServiceEvent>,
    pub(in super::super) listener: Async<TcpListener>,
    pub(in super::super) acceptor: TlsAcceptor,
    pub(in super::super) data: Vec<NativeDescription>,
    pub(in super::super) cloud: Arc<Mutex<CloudState>>,
    pub(in super::super) clock: Arc<TestClock>,
    stall_timeout: Duration,
}
pub(in super::super) struct Control {
    pub(in super::super) uid: String,
    pub(in super::super) publication: Rc<crate::app::App>,
    pub(in super::super) ready: Option<TopologyReady>,
    pub(in super::super) store: Store,
    pub(in super::super) context: Rc<crate::xiaomi::adapter::state::Context>,
    pub(in super::super) cloud_client: CloudClient,
    pub(in super::super) discovery: Discovery,
    pub(in super::super) events: flume::Sender<mdns_sd::ServiceEvent>,
    pub(in super::super) listener: Async<TcpListener>,
    pub(in super::super) acceptor: TlsAcceptor,
    pub(in super::super) data: Vec<NativeDescription>,
    pub(in super::super) cloud: Arc<Mutex<CloudState>>,
    pub(in super::super) clock: Arc<TestClock>,
}
impl Rig {
    pub(in super::super) fn reopen_adapter(&self) -> (XiaomiAdapter, TopologyReady) {
        self.network
            .build(
                self.store.clone(),
                self.source.cloud.clone(),
                self.discovery.clone(),
                self.clock.clone(),
            )
            .unwrap()
    }

    pub(in super::super) fn new(restore: bool, members: &[&str]) -> Self {
        Self::build(restore, members, None)
    }
    pub(in super::super) fn enable_recovery(&self) {
        use crate::xiaomi::auth::{AccountClientIdentity, AccountCredentials};
        let mut record = self.store.xiaomi().load().unwrap().unwrap();
        record.recovery = crate::storage::AccountRecovery {
            credentials: AccountCredentials::new(
                record.uid.clone(),
                "cuser".into(),
                "pass".into(),
                AccountClientIdentity::new("device-id".into(), "test-agent".into()).unwrap(),
            )
            .unwrap(),
            stop: None,
        };
        self.store.xiaomi().fixture_replace(&record).unwrap();
        self.source.context.adopt_credentials(record);
    }
    /// A fresh catalog containing one real on/off light among ordinary devices.
    pub(in super::super) fn unrestored_with_member_light(members: &[&str]) -> Self {
        let instance = json!({
            "type": "urn:miot-spec-v2:device:light:0000A001:example:1",
            "services": [{
                "iid": 2, "type": "urn:miot-spec-v2:service:light:00007802:example:1",
                "properties": [{"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1",
                    "format": "bool", "access": ["read", "write", "notify"]}]
            }]
        });
        let rig = Self::build_with(
            false,
            members,
            None,
            None,
            &[("one", "example.light", instance)],
            &[],
        );
        let outlet: Value = serde_json::from_str(include_str!(
            "../../../../../tests/fixtures/miot/outlet.json"
        ))
        .unwrap();
        rig.cloud
            .lock()
            .unwrap()
            .catalog_instances
            .insert(outlet["type"].as_str().unwrap().into(), outlet);
        rig
    }
    pub(in super::super) fn light(model: &str) -> Self {
        Self::build(true, &["one"], Some(model))
    }
    /// A fixture-derived independent light with one of the four standard profiles.
    pub(in super::super) fn profile_light(model: &str, instance: Value) -> Self {
        Self::build_with(true, &["one"], Some(model), Some(instance), &[], &[])
    }
    /// Several fixture-derived lights with their own DIDs; each gets its real
    /// parsed description and the catalog can serve its instance type.
    pub(in super::super) fn lights(models: &[(&str, &str)]) -> Self {
        let members = models.iter().map(|(did, _)| *did).collect::<Vec<_>>();
        let instances = models
            .iter()
            .map(|(did, model)| {
                (
                    *did,
                    *model,
                    serde_json::from_str(light_fixture(model)).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        Self::build_with(true, &members, None, None, &instances, &[])
    }
    /// A scenario that needs more real Matter startup time under parallel tests.
    pub(in super::super) fn with_stall_timeout(mut self, timeout: Duration) -> Self {
        self.stall_timeout = timeout;
        self
    }

    /// Temperature/humidity sensor members with the public t6 fixture.
    pub(in super::super) fn sensors(members: &[&str]) -> Self {
        let instances = members
            .iter()
            .map(|did| (*did, super::sensor_native(did)))
            .collect::<Vec<_>>();
        Self::build_with(true, members, None, None, &[], &instances)
    }

    /// A restored sensor whose stored native facts have no battery capability.
    pub(in super::super) fn sensor_without_battery(did: &str) -> Self {
        Self::build_with(
            true,
            &[did],
            None,
            None,
            &[],
            &[(did, super::sensor_native_without_battery(did))],
        )
    }

    /// Lights with caller-provided parsed instances, for range/boundary cases.
    pub(in super::super) fn custom_lights(lights: &[(&str, &str, Value)]) -> Self {
        let members = lights.iter().map(|(did, _, _)| *did).collect::<Vec<_>>();
        Self::build_with(true, &members, None, None, lights, &[])
    }
    fn build(restore: bool, members: &[&str], light: Option<&str>) -> Self {
        Self::build_with(restore, members, light, None, &[], &[])
    }
    fn build_with(
        restore: bool,
        members: &[&str],
        light: Option<&str>,
        custom: Option<Value>,
        lights: &[(&str, &str, Value)],
        natives: &[(&str, SourceFixture)],
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let tls = TlsFixture::new(false);
        let uid = tls.credentials.uid.clone();
        let mut credentials = tls.credentials.clone();
        credentials.recovery.stop = Some(crate::storage::RecoveryStop::Rejected);
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        credentials.ha.authorization.tokens.refresh_at = now + 1_000_000;
        credentials.ha.authorization.tokens.expires_at = now + 2_000_000;
        credentials.mijia = crate::xiaomi::mijia::MijiaSession::new(
            uid.clone(),
            "device-session".into(),
            "c2VjdXJpdHk=".into(),
            "test-agent".into(),
        )
        .unwrap();
        store.xiaomi().fixture_replace(&credentials).unwrap();
        let mut data = vec![
            cached(&uid, "one", false),
            cached(&uid, "multi", true),
            cached(&uid, "two", false),
        ];
        for did in members {
            if !data.iter().any(|record| record.device().key().id == *did) {
                data.push(cached(&uid, did, false));
            }
        }
        if let Some(model) = light {
            let instance =
                custom.unwrap_or_else(|| serde_json::from_str(light_fixture(model)).unwrap());
            data[0] = SourceFixture::new("one", model, instance);
        }
        for (did, model, instance) in lights {
            let record = data
                .iter_mut()
                .find(|record| record.device().key().id == *did)
                .unwrap();
            *record = SourceFixture::new(did, model, instance.clone());
        }
        for (did, native) in natives {
            let record = data
                .iter_mut()
                .find(|record| record.device().key().id == *did)
                .unwrap_or_else(|| panic!("missing DID {did}"));
            *record = (*native).clone();
        }
        if restore {
            let natives = data
                .iter()
                .filter(|record| members.contains(&record.device().key().id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            commit_natives(&store, &uid, &natives);
        }
        let instances = data
            .iter()
            .map(|source| {
                (
                    source.raw.instance_type.clone(),
                    source.raw.instance.clone(),
                )
            })
            .collect::<Vec<_>>();
        let data = data
            .into_iter()
            .map(|source| source.native)
            .collect::<Vec<_>>();
        let cloud = Arc::new(Mutex::new(CloudState {
            members: members.iter().map(|v| v.to_string()).collect(),
            status: 200,
            home_status: 200,
            detail_status: 200,
            cert_status: 200,
            token_status: 200,
            restore_status: 200,
            token_lifetime: 1_000_000,
            property_status: 200,
            read_status: 200,
            rpc_status: 200,
            action_status: 200,
            info_status: 200,
            catalog_status: 200,
            ..Default::default()
        }));
        for (urn, instance) in instances {
            cloud
                .lock()
                .unwrap()
                .catalog_instances
                .insert(urn, instance);
        }
        let info_dids = if restore {
            members
                .iter()
                .map(|did| (*did).to_string())
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let responses = cloud.clone();
        let records = data.clone();
        let account_uid = uid.clone();
        let certificate = tls
            .credentials
            .hub
            .as_ref()
            .unwrap()
            .certificate_pem
            .clone()
            .unwrap();
        let (base, _requests) = dynamic_mock_server(200, move |request| {
            let detail_finished = responses.clone();
            let token_finished = responses.clone();
            let mut state = responses.lock().unwrap();
            if request.target.ends_with("/gethome")
                && request
                    .headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer")
            {
                state.ha_homes += 1;
                return MockResponse::json(503, "unexpected HA identity probe");
            }
            if state.status != 200 {
                return MockResponse::json(state.status, "{}");
            }
            let data = || {
                let data = url::form_urlencoded::parse(request.body.as_bytes())
                    .find_map(|(key, value)| (key == "data").then(|| value.into_owned()))
                    .unwrap();
                serde_json::from_str::<Value>(&data).unwrap()
            };
            if request.target.starts_with("/pass/serviceLogin?") {
                state.restores += 1;
                let (status, body) = match state.restore_status {
                    401 => (200, json!({"code":70002})),
                    403 => (
                        200,
                        json!({"notificationUrl":"https://account.xiaomi.com/verify?ticket=test"}),
                    ),
                    status => (
                        status,
                        json!({"code":0,"userId":account_uid,"cUserId":"new-cuser","passToken":"new-pass","ssecurity":"bmV3LXNlY3VyaXR5","location":"https://sts.api.io.mi.com/sts?ticket=test"}),
                    ),
                };
                let mut response = MockResponse::json(status, &body.to_string());
                if let Some(gate) = state.restore_gate.clone() {
                    response = response.gated(gate);
                }
                return response;
            }
            if request.target.starts_with("/sts?") {
                state.sts_requests += 1;
                return MockResponse::raw(
                    "HTTP/1.1 200 Test\r\nSet-Cookie: serviceToken=restored-service; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
            if request.target.contains("/app/home/rpc/") {
                let body = data();
                let did = request
                    .target
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_owned();
                let method = body["method"].as_str().unwrap().to_owned();
                state
                    .rpc_reads
                    .push((did, method.clone(), body["params"].clone()));
                let result = state
                    .rpc_results
                    .get(&method)
                    .cloned()
                    .unwrap_or_else(|| json!([null, null, null]));
                let mut response = MockResponse::json(
                    state.rpc_status,
                    &json!({"code":0,"id":body["id"],"result":result}).to_string(),
                );
                if let Some(gate) = state.rpc_gate.clone() {
                    response = response.gated(gate);
                }
                return response;
            }
            if request.target.ends_with("/miotspec/action") {
                let body = data();
                let params = &body["params"];
                state.action_requests.push((
                    params["did"].as_str().unwrap().into(),
                    params["siid"].as_u64().unwrap() as u32,
                    params["aiid"].as_u64().unwrap() as u32,
                    params["in"].clone(),
                ));
                let result = json!({
                    "did": params["did"],
                    "siid": params["siid"],
                    "aiid": params["aiid"],
                    "code": state.action_code,
                    "out": [],
                });
                let mut response = MockResponse::json(
                    state.action_status,
                    &json!({"code":0,"result":result}).to_string(),
                );
                if let Some(gate) = state.action_gate.clone() {
                    response = response.gated(gate);
                }
                return response;
            }
            let value = if request.target.ends_with("/miotspec/prop/get") {
                let body = data();
                let mut batch = Vec::new();
                let mut readings = Vec::new();
                let mut gate = None;
                for item in body["params"].as_array().unwrap() {
                    let key = (
                        item["did"].as_str().unwrap().to_string(),
                        item["siid"].as_u64().unwrap() as u32,
                        item["piid"].as_u64().unwrap() as u32,
                    );
                    state.property_reads.push(key.clone());
                    batch.push(key.clone());
                    let value = state.raw_values.get(&key).cloned().unwrap_or_else(|| {
                        json!(state.read_values.get(&key).copied().unwrap_or(false))
                    });
                    let code = state.read_codes.get(&key).copied().unwrap_or(0);
                    let mut reading = json!({"did":item["did"],"siid":item["siid"],"piid":item["piid"],"code":code,"value":value});
                    if let Some(update_time) = state.update_times.get(&key) {
                        reading["updateTime"] = json!(update_time);
                    }
                    if gate.is_none() {
                        gate = state.read_gates.get(&key).cloned();
                    }
                    readings.push(reading);
                }
                state.read_batches.push(batch);
                let result =
                    json!({"code":0,"result":state.read_results.as_ref().unwrap_or(&readings)});
                let mut response = MockResponse::json(state.read_status, &result.to_string());
                if let Some(gate) = gate {
                    response = response.gated(gate);
                }
                return response;
            } else if request.target.ends_with("/miotspec/prop/set") {
                let body = data();
                let items = body["params"].as_array().unwrap();
                let did = items[0]["did"].as_str().unwrap();
                let native_group = state.catalog_groups.iter().any(|record| {
                    record["extra"]["member_list"]["group_did"].as_str() == Some(did)
                });
                let light = native_group
                    || records.iter().any(|record| {
                        record.device().key().id == did
                            && (record.device().functions()[0].lighting().is_some()
                                || record.device().functions()[0].curtain().is_some()
                                || record.device().functions()[0].fan().is_some()
                                || record.device().functions()[0].air_conditioner().is_some())
                    });
                if light {
                    state
                        .numeric_writes
                        .push((items.clone(), request.headers.clone()));
                } else {
                    let item = &items[0];
                    state.property_writes.push((
                        item["did"].as_str().unwrap().into(),
                        item["siid"].as_u64().unwrap() as u32,
                        item["piid"].as_u64().unwrap() as u32,
                        item["value"].as_bool().unwrap(),
                        request.headers.clone(),
                    ));
                }
                let status = state.property_status;
                let result = json!({"code":0,"result":items.iter().map(|item| {
                    let key=(item["did"].as_str().unwrap().to_owned(),item["siid"].as_u64().unwrap() as u32,item["piid"].as_u64().unwrap() as u32);
                    json!({"did":item["did"],"siid":item["siid"],"piid":item["piid"],"code":state.property_codes.get(&key).copied().unwrap_or(state.property_code)})
                }).collect::<Vec<_>>()});
                let per_did = items.iter().find_map(|item| {
                    state
                        .write_gates
                        .get(item["did"].as_str().unwrap())
                        .cloned()
                });
                let mut response = MockResponse::json(status, &result.to_string());
                if let Some(gate) = per_did {
                    response = response.gated(gate);
                } else if let Some(gate) = state.property_gate.clone() {
                    response = response.gated(gate);
                }
                return response;
            } else if request.target.contains("get_token") {
                state.tokens += 1;
                if state.token_status != 200 {
                    let mut response = MockResponse::json(state.token_status, "{}")
                        .on_sent(move || token_finished.lock().unwrap().token_done += 1);
                    if let Some(gate) = state.token_gate.clone() {
                        response = response.gated(gate);
                    }
                    return response;
                }
                json!({"code":0,"result":{"access_token":state.token_access.as_deref().unwrap_or("fresh-access"),"refresh_token":"fresh-refresh","expires_in":state.token_lifetime}})
            } else if request.target.ends_with("/get_central_crt") {
                state.certs += 1;
                if state.cert_status != 200 {
                    let mut response = MockResponse::json(state.cert_status, "{}");
                    if let Some(gate) = state.cert_gate.clone() {
                        response = response.gated(gate);
                    }
                    return response;
                }
                if state.cert_sign_csr {
                    let body: Value = serde_json::from_str(&request.body).unwrap();
                    let csr = STANDARD.decode(body["csr"].as_str().unwrap()).unwrap();
                    let csr = String::from_utf8(csr).unwrap();
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    let signed =
                        crate::xiaomi::tests::support::sign_csr(&csr, now - 60, now + 864_000)
                            .unwrap();
                    json!({"code":0,"result":{"cert":signed}})
                } else {
                    json!({"code":0,"result":{"cert":state.cert_pem.as_deref().unwrap_or(&certificate)}})
                }
            } else if request.target.ends_with("/get_dev_room_page") {
                state.member_pages += 1;
                json!({"code":0,"result":{"info":[{"id":"local","dids":[]}],"has_more":false}})
            } else if request.target.ends_with("/gethome") {
                state.homes += 1;
                state
                    .home_requests
                    .push((request.headers.clone(), request.body.clone()));
                if state.home_bad_signature {
                    return MockResponse::json(200, r#"{"code":1,"message":"invalid signature"}"#);
                }
                if state.home_status != 200 {
                    let mut response = MockResponse::json(state.home_status, "{}");
                    if let Some(gate) = state.home_gate.clone() {
                        response = response.gated(gate);
                    }
                    return response;
                }
                if state.partial {
                    json!({"code":0,"result":{"homelist":[],"has_more":true}})
                } else if state.missing_home {
                    json!({"code":0,"result":{"homelist":[],"has_more":false}})
                } else {
                    let mut result = json!({"homelist":[{"id":if state.other_home {"other"} else {"local"},"uid":if state.foreign {"foreign"}else {&account_uid},"roomlist":[],"dids":state.members}]});
                    if state.home_more {
                        result["has_more"] = json!(true);
                        result["max_id"] = json!("next-page");
                    }
                    json!({"code":0,"result":result})
                }
            } else if request.target.ends_with("/device_list_page") {
                let body = data();
                let requested = body.get("dids").and_then(Value::as_array);
                if requested.is_none() {
                    // Full unfiltered account catalog. Ordinary home members are
                    // returned together with injectable real group records.
                    state.catalogs += 1;
                    if state.catalog_status != 200 {
                        return MockResponse::json(state.catalog_status, "{}");
                    }
                    let mut list = records
                        .iter()
                        .filter(|record| state.members.contains(&record.device().key().id))
                        .map(|record| {
                            state
                                .catalog_overrides
                                .get(&record.device().key().id)
                                .cloned()
                                .unwrap_or_else(|| cloud_record(&state, record))
                        })
                        .collect::<Vec<_>>();
                    list.extend(state.catalog_groups.iter().cloned());
                    let value = json!({"code":0,"result":{"list":list,"has_more":state.catalog_partial,"next_start_did":Value::Null}});
                    return MockResponse::json(200, &value.to_string());
                }
                if requested
                    .unwrap()
                    .iter()
                    .all(|did| info_dids.contains(did.as_str().unwrap()))
                {
                    state.infos += 1;
                    let value = json!({"code":0,"result":{"list":records.iter().filter(|record|state.members.contains(&record.device().key().id)).map(|record|cloud_record(&state, record)).collect::<Vec<_>>(),"has_more":state.info_partial}});
                    let mut response = MockResponse::json(state.info_status, &value.to_string());
                    if let Some(gate) = state.info_gate.clone() {
                        response = response.gated(gate);
                    }
                    return response;
                }
                state.details += 1;
                state.detail_requests.push(
                    requested
                        .unwrap()
                        .iter()
                        .map(|did| did.as_str().unwrap().to_owned())
                        .collect(),
                );
                if state.detail_status != 200 {
                    let mut response = MockResponse::json(state.detail_status, "{}")
                        .delayed(state.detail_delay)
                        .on_sent(move || detail_finished.lock().unwrap().detail_done += 1);
                    if let Some(gate) = state.detail_gate.clone() {
                        response = response.gated(gate);
                    }
                    return response;
                }
                json!({"code":0,"result":{"list":records.iter().filter(|record|state.members.contains(&record.device().key().id)).map(|record|cloud_record(&state, record)).collect::<Vec<_>>()}})
            } else if request.target.starts_with("/miot-spec-v2/instance?") {
                let query = request
                    .target
                    .split_once('?')
                    .map(|(_, query)| query)
                    .unwrap_or("");
                let requested_type = url::form_urlencoded::parse(query.as_bytes())
                    .find_map(|(key, value)| (key == "type").then(|| value.into_owned()));
                if let Some(urn) = &requested_type {
                    *state.instance_reads.entry(urn.clone()).or_default() += 1;
                }
                if let Some(status) = requested_type
                    .as_ref()
                    .and_then(|urn| state.instance_status.get(urn))
                    && *status != 200
                {
                    return MockResponse::json(*status, "{}");
                }
                if let Some(instance) = requested_type
                    .as_ref()
                    .and_then(|urn| state.catalog_instances.get(urn))
                {
                    let mut instance = instance.clone();
                    if records.iter().any(|record| {
                        requested_type.as_deref() == Some(record.instance_type())
                            && state.unsupported.contains(&record.device().key().id)
                    }) {
                        instance["services"] = json!([]);
                    }
                    let response = MockResponse::json(200, &instance.to_string());
                    return match state.instance_gates.get(requested_type.as_ref().unwrap()) {
                        Some(gate) => response.gated(gate.clone()),
                        None => response,
                    };
                }
                return MockResponse::json(404, "{}");
            } else {
                return MockResponse::json(503, "{}");
            };
            let response = MockResponse::json(200, &value.to_string());
            if request.target.ends_with("/gethome") {
                {
                    let mut response = response.delayed(state.home_delay);
                    if let Some(gate) = state.home_gate.clone() {
                        response = response.gated(gate);
                    }
                    response
                }
            } else if request.target.contains("get_token") {
                let mut response = response
                    .delayed(state.token_delay)
                    .on_sent(move || token_finished.lock().unwrap().token_done += 1);
                if let Some(gate) = state.token_gate.clone() {
                    response = response.gated(gate);
                }
                response
            } else if request.target.ends_with("/device_list_page") {
                response.delayed(state.detail_delay)
            } else if request.target.ends_with("/get_central_crt") {
                if let Some(gate) = state.cert_gate.clone() {
                    response.gated(gate)
                } else {
                    response
                }
            } else {
                response
            }
        });
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let (events, receiver) = flume::unbounded();
        let (discovery, driver) = Discovery::with_events(receiver);
        let clock = Arc::new(TestClock::default());
        clock
            .automatic
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let network = TestNetwork::new(Some(tls.ca_pem.clone()));
        let (source, ready) = network
            .build(store.clone(), client, discovery.clone(), clock.clone())
            .unwrap();
        Self {
            uid,
            directory: Rc::new(directory),
            source,
            network,
            ready,
            store,
            discovery,
            driver,
            events,
            listener,
            acceptor: TlsAcceptor::from(tls.server_config()),
            data,
            cloud,
            clock,
            stall_timeout: Duration::from_secs(5),
        }
    }
    pub(in super::super) async fn run<F, Fut>(self, test: F)
    where
        F: FnOnce(Control) -> Fut,
        Fut: Future<Output = ()>,
    {
        self.run_result(test).await.unwrap();
    }
    pub(in super::super) async fn run_result<F, Fut>(self, test: F) -> Result<(), AdapterError>
    where
        F: FnOnce(Control) -> Fut,
        Fut: Future<Output = ()>,
    {
        self.run_result_mode(true, |_| {}, test).await
    }
    pub(in super::super) async fn run_with_status<F, Fut>(
        self,
        on_status: impl FnMut(AdapterStatus),
        test: F,
    ) where
        F: FnOnce(Control) -> Fut,
        Fut: Future<Output = ()>,
    {
        self.run_result_mode(true, on_status, test).await.unwrap();
    }
    /// Run with the production publication applier disabled; the caller drives
    /// the real configured runtime as the sole publisher.
    pub(in super::super) async fn run_result_without_publication<F, Fut>(
        self,
        test: F,
    ) -> Result<(), AdapterError>
    where
        F: FnOnce(Control) -> Fut,
        Fut: Future<Output = ()>,
    {
        self.run_result_mode(false, |_| {}, test).await
    }
    async fn run_result_mode<F, Fut>(
        self,
        autoapply: bool,
        on_status: impl FnMut(AdapterStatus),
        test: F,
    ) -> Result<(), AdapterError>
    where
        F: FnOnce(Control) -> Fut,
        Fut: Future<Output = ()>,
    {
        let keep_events = self.events.clone();
        let context = self.source.context.clone();
        let clock = self.clock.clone();
        let stall_timeout = self.stall_timeout;
        let publication = Rc::new(
            crate::app::App::open(
                super::super::super::publication::SourceHandle(context.clone()),
                self.store.clone(),
            )
            .unwrap(),
        );
        let applying = async {
            if !autoapply {
                return future::pending::<Result<(), AdapterError>>().await;
            }
            loop {
                let changed = publication.listen();
                match publication.plan() {
                    Ok(Some(plan)) => {
                        if plan.rebuild {
                            publication.stop_actions();
                        }
                        let prepared = publication.prepare(plan);
                        let _ = publication.apply(prepared);
                    }
                    Ok(None) => {}
                    Err(error) => publication.fail(error.to_string()),
                }
                changed.await;
            }
        };
        let discovering = async {
            self.driver.run().await?;
            future::pending::<Result<(), AdapterError>>().await
        };
        let testing = test(Control {
            uid: self.uid.clone(),
            ready: Some(self.ready),
            publication: publication.clone(),
            cloud_client: self.source.cloud.clone(),
            store: self.store,
            context,
            discovery: self.discovery,
            events: self.events,
            listener: self.listener,
            acceptor: self.acceptor,
            data: self.data,
            cloud: self.cloud,
            clock: self.clock,
        });
        let running = self.source.run(on_status);
        let ticking = async {
            loop {
                async_io::Timer::after(Duration::from_millis(1)).await;
                if clock.automatic.load(std::sync::atomic::Ordering::Relaxed) {
                    clock.advance(Duration::from_millis(10));
                }
            }
        };
        let result = future::or(
            ticking,
            future::or(
                future::or(
                    future::or(future::or(running, applying), discovering),
                    async {
                        testing.await;
                        Ok(())
                    },
                ),
                async {
                    async_io::Timer::after(stall_timeout).await;
                    panic!("source scenario stalled after {stall_timeout:?}");
                },
            ),
        )
        .await;
        drop(keep_events);
        result
    }
}
impl Control {
    pub(in super::super) fn reopen_adapter(&self) -> (XiaomiAdapter, TopologyReady) {
        TestNetwork::new(None)
            .build(
                self.store.clone(),
                self.cloud_client.clone(),
                self.discovery.clone(),
                self.clock.clone(),
            )
            .unwrap()
    }

    pub(in super::super) fn announce(&self) {
        self.events
            .send(announcement(
                self.listener.get_ref().local_addr().unwrap(),
                &self.uid,
                "hub",
                123,
            ))
            .unwrap();
    }
    pub(in super::super) async fn accept(&self) -> Broker {
        let (socket, _) = self.listener.accept().await.unwrap();
        Broker::new(self.acceptor.accept(socket).await.unwrap())
    }
    pub(in super::super) async fn peer<F, Fut>(&self, peer: Rc<Peer>, test: F)
    where
        F: FnOnce(Rc<Peer>) -> Fut,
        Fut: Future<Output = ()>,
    {
        let mut broker = self.accept().await;
        broker.setup().await;
        peer.state.borrow_mut().client_id = broker.client_id.clone();
        // The peer remains alive until every scenario assertion has completed.
        future::or(
            peer_run(broker, peer.clone(), self.data.clone()),
            test(peer),
        )
        .await;
    }
}

fn cloud_record(state: &CloudState, record: &NativeDescription) -> Value {
    let did = &record.device().key().id;
    json!({
        "did": did,
        "name": state.names.get(did).cloned().unwrap_or_else(|| format!("name {did}")),
        "model": state.models.get(did).map(String::as_str).unwrap_or(record.model()),
        "spec_type": state.instance_types.get(did).map(String::as_str).unwrap_or(record.instance_type()),
        "isOnline": state.online.contains(did),
    })
}

fn light_fixture(model: &str) -> &'static str {
    match model {
        "devcea.light.ls2307" => {
            include_str!("../../../../../tests/fixtures/miot/devcea-light-ls2307.json")
        }
        "yeelink.light.light3" => {
            include_str!("../../../../../tests/fixtures/miot/yeelink-light-light3.json")
        }
        "yeelink.light.ml9" => {
            include_str!("../../../../../tests/fixtures/miot/yeelink-light-ml9.json")
        }
        "yeelink.light.spot2" => {
            include_str!("../../../../../tests/fixtures/miot/yeelink-light-spot2.json")
        }
        "xiaomi.light.ceil04" => {
            include_str!("../../../../../tests/fixtures/miot/xiaomi-light-ceil04.json")
        }
        "xiaomi.light.bar2" => {
            include_str!("../../../../../tests/fixtures/miot/xiaomi-light-bar2.json")
        }
        "lemesh.light.wy0c15" => {
            include_str!("../../../../../tests/fixtures/miot/lemesh-light-wy0c15.json")
        }
        _ => panic!("unknown light fixture"),
    }
}
