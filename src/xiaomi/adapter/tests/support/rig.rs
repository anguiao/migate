use super::*;

#[derive(Default)]
pub(in super::super) struct GroupFixture {
    pub(in super::super) members: Vec<String>,
    pub(in super::super) physical: bool,
    pub(in super::super) missing: bool,
}

/// One cloud group relation in a rig's selected home.
pub(in super::super) struct GroupSpec<'a> {
    pub(in super::super) did: &'a str,
    pub(in super::super) model: &'a str,
    pub(in super::super) members: &'a [&'a str],
    pub(in super::super) label: &'a str,
}

#[derive(Default)]
pub(in super::super) struct CloudState {
    pub(in super::super) members: Vec<String>,
    pub(in super::super) groups: BTreeMap<String, GroupFixture>,
    pub(in super::super) names: BTreeMap<String, String>,
    pub(in super::super) status: u16,
    pub(in super::super) home_delay: Duration,
    pub(in super::super) home_gate: Option<flume::Receiver<()>>,
    pub(in super::super) home_status: u16,
    pub(in super::super) foreign: bool,
    pub(in super::super) missing_home: bool,
    pub(in super::super) other_home: bool,
    pub(in super::super) partial: bool,
    pub(in super::super) unsupported: BTreeSet<String>,
    pub(in super::super) detail_status: u16,
    /// Per-DID device-description HTTP failures that leave other reads intact.
    pub(in super::super) detail_failures: BTreeMap<String, u16>,
    /// Per-DID device-description response gates for partial-refresh tests.
    pub(in super::super) detail_gates: BTreeMap<String, flume::Receiver<()>>,
    /// DIDs whose directory record is absent from device-information responses
    /// while the owned-home membership still confirms them.
    pub(in super::super) missing_records: BTreeSet<String>,
    /// Device-description requests observed for each DID.
    pub(in super::super) detail_counts: BTreeMap<String, usize>,
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
    pub(in super::super) raw_values: BTreeMap<(String, u32, u32), Value>,
    pub(in super::super) property_codes: BTreeMap<(String, u32, u32), i64>,
    pub(in super::super) numeric_writes: Vec<(Vec<Value>, String)>,
    pub(in super::super) read_values: BTreeMap<(String, u32, u32), bool>,
    pub(in super::super) read_gates: BTreeMap<(String, u32, u32), flume::Receiver<()>>,
    pub(in super::super) read_status: u16,
}
pub(in super::super) struct Rig {
    pub(in super::super) _directory: tempfile::TempDir,
    pub(in super::super) source: XiaomiAdapter,
    pub(in super::super) ready: TopologyReady,
    pub(in super::super) store: Store,
    pub(in super::super) discovery: Discovery,
    pub(in super::super) driver: crate::xiaomi::discovery::DiscoveryDriver,
    pub(in super::super) events: flume::Sender<mdns_sd::ServiceEvent>,
    pub(in super::super) listener: Async<TcpListener>,
    pub(in super::super) acceptor: TlsAcceptor,
    pub(in super::super) data: Vec<CachedPhysicalDevice>,
    pub(in super::super) cloud: Arc<Mutex<CloudState>>,
    pub(in super::super) clock: Arc<TestClock>,
}
pub(in super::super) struct Control {
    pub(in super::super) ready: Option<TopologyReady>,
    pub(in super::super) store: Store,
    pub(in super::super) context: Rc<crate::xiaomi::adapter::state::Context>,
    pub(in super::super) discovery: Discovery,
    pub(in super::super) events: flume::Sender<mdns_sd::ServiceEvent>,
    pub(in super::super) listener: Async<TcpListener>,
    pub(in super::super) acceptor: TlsAcceptor,
    pub(in super::super) data: Vec<CachedPhysicalDevice>,
    pub(in super::super) cloud: Arc<Mutex<CloudState>>,
    pub(in super::super) clock: Arc<TestClock>,
}
impl Rig {
    pub(in super::super) fn new(restore: bool, members: &[&str]) -> Self {
        Self::build(restore, members, None)
    }
    pub(in super::super) fn light(model: &str) -> Self {
        Self::build(true, &["one"], Some(model))
    }
    /// A restored scope with published single-light members and one confirmed
    /// physical group relation already committed before adapter bootstrap.
    pub(in super::super) fn restored_group(
        members: &[&str],
        did: &str,
        model: &str,
        label: &str,
    ) -> Self {
        Self::build_groups(
            true,
            &[],
            &[GroupSpec {
                did,
                model,
                members,
                label,
            }],
            Some("yeelink.light.light3"),
            &[],
        )
    }
    /// A live selected home whose cloud directory confirms one group relation.
    pub(in super::super) fn group(members: &[&str], did: &str, model: &str) -> Self {
        Self::build_groups(
            false,
            &[],
            &[GroupSpec {
                did,
                model,
                members,
                label: "Reading lamp",
            }],
            Some("yeelink.light.light3"),
            &[],
        )
    }
    /// A live selected home whose cloud directory confirms several group relations.
    pub(in super::super) fn groups(groups: &[GroupSpec<'_>]) -> Self {
        Self::build_groups(false, &[], groups, Some("yeelink.light.light3"), &[])
    }
    /// A live selected home with one group whose members use distinct light models.
    pub(in super::super) fn group_mixed(members: &[(&str, &str)], did: &str, model: &str) -> Self {
        let member_dids = members.iter().map(|(did, _)| *did).collect::<Vec<_>>();
        Self::build_groups(
            false,
            &[],
            &[GroupSpec {
                did,
                model,
                members: &member_dids,
                label: "Mixed group",
            }],
            None,
            members,
        )
    }
    fn build(restore: bool, members: &[&str], light: Option<&str>) -> Self {
        Self::build_groups(restore, members, &[], light, &[])
    }
    fn build_groups(
        restore: bool,
        members: &[&str],
        groups: &[GroupSpec<'_>],
        light: Option<&str>,
        overrides: &[(&str, &str)],
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let tls = TlsFixture::new(false);
        let mut credentials = tls.credentials.clone();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        credentials.tokens.refresh_at = now + 1_000_000;
        credentials.tokens.expires_at = now + 2_000_000;
        store.xiaomi().replace(&credentials).unwrap();
        let group_ids = groups
            .iter()
            .map(|group| group.did.to_string())
            .collect::<BTreeSet<_>>();
        let mut record_members = BTreeSet::new();
        record_members.extend(members.iter().map(|did| (*did).to_string()));
        for group in groups {
            for member in group.members {
                // A nested group DID is a relation source, never a light member.
                if !group_ids.contains(*member) {
                    record_members.insert((*member).to_string());
                }
            }
        }
        let mut data = vec![
            cached(&tls.credentials.uid, "one", false),
            cached(&tls.credentials.uid, "multi", true),
            cached(&tls.credentials.uid, "two", false),
        ];
        for did in &record_members {
            if !data.iter().any(|record| record.device.key.id == *did) {
                data.push(cached(&tls.credentials.uid, did, false));
            }
        }
        for group in groups {
            debug_assert!(!data.iter().any(|record| record.device.key.id == group.did));
            // Group records are cloud metadata carriers only; never parsed as MIoT devices.
            let mut record = cached(&tls.credentials.uid, "group-placeholder", false);
            record.device.key = DeviceKey::new("xiaomi", group.did);
            record.model = group.model.into();
            data.push(record);
        }
        if let Some(model) = light {
            let text = light_fixture(model);
            let instance: Value = serde_json::from_str(text).unwrap();
            for did in &record_members {
                let record = data
                    .iter_mut()
                    .find(|record| record.device.key.id == *did)
                    .unwrap();
                // The default multi-function outlet stays a deliberate
                // unsupported member rather than being rewritten as a light.
                if record.model == "cuco.plug.cp7pd" {
                    continue;
                }
                record.model = model.into();
                record.instance_type = instance["type"].as_str().unwrap().into();
                record.device = crate::xiaomi::capability::parse_device_for_model(
                    DeviceKey::new("xiaomi", did),
                    model,
                    &instance,
                )
                .unwrap();
            }
        }
        for (did, model) in overrides {
            let text = light_fixture(model);
            let instance: Value = serde_json::from_str(text).unwrap();
            let record = data
                .iter_mut()
                .find(|record| record.device.key.id == *did)
                .unwrap();
            record.model = (*model).into();
            record.instance_type = instance["type"].as_str().unwrap().into();
            record.device = crate::xiaomi::capability::parse_device_for_model(
                DeviceKey::new("xiaomi", *did),
                model,
                &instance,
            )
            .unwrap();
        }
        if restore {
            store
                .devices()
                .save_scope(&scope(&tls.credentials.uid))
                .unwrap();
            for record in &data {
                if record_members.contains(&record.device.key.id) {
                    store
                        .devices()
                        .publish_cached(record, &format!("name {}", record.device.key.id))
                        .unwrap();
                }
            }
            for group in groups {
                if group.members.is_empty() {
                    continue;
                }
                let composition = crate::device::Composition {
                    key: DeviceKey::new("xiaomi", group.did),
                    label: group.label.into(),
                    members: group
                        .members
                        .iter()
                        .map(|member| DeviceKey::new("xiaomi", *member))
                        .collect(),
                };
                store
                    .devices()
                    .commit_composition(&scope(&tls.credentials.uid), &[composition], &[])
                    .unwrap();
            }
        }
        let home = record_members
            .iter()
            .cloned()
            .chain(groups.iter().map(|group| group.did.to_string()))
            .collect::<Vec<_>>();
        let cloud = Arc::new(Mutex::new(CloudState {
            members: home.clone(),
            groups: groups
                .iter()
                .map(|group| {
                    (
                        group.did.to_string(),
                        GroupFixture {
                            members: group
                                .members
                                .iter()
                                .map(|member| (*member).to_string())
                                .collect(),
                            physical: true,
                            missing: false,
                        },
                    )
                })
                .collect(),
            status: 200,
            home_status: 200,
            detail_status: 200,
            cert_status: 200,
            token_status: 200,
            token_lifetime: 1_000_000,
            property_status: 200,
            read_status: 200,
            info_status: 200,
            ..Default::default()
        }));
        let info_dids = if restore {
            home.iter()
                .map(|did| (*did).to_string())
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let responses = cloud.clone();
        let records = data.clone();
        let certificate = tls.credentials.certificate_pem.clone();
        let (base, _requests) = dynamic_mock_server(200, move |request| {
            let detail_finished = responses.clone();
            let token_finished = responses.clone();
            let mut state = responses.lock().unwrap();
            if state.status != 200 {
                return MockResponse::json(state.status, "{}");
            }
            let value = if request.target.ends_with("/miotspec/prop/get") {
                let body: Value = serde_json::from_str(&request.body).unwrap();
                let item = &body["params"][0];
                let key = (
                    item["did"].as_str().unwrap().to_string(),
                    item["siid"].as_u64().unwrap() as u32,
                    item["piid"].as_u64().unwrap() as u32,
                );
                state.property_reads.push(key.clone());
                let value = state.raw_values.get(&key).cloned().unwrap_or_else(|| {
                    json!(state.read_values.get(&key).copied().unwrap_or(false))
                });
                let result = json!({"code":0,"result":[{"did":item["did"],"siid":item["siid"],"piid":item["piid"],"code":0,"value":value}]});
                let mut response = MockResponse::json(state.read_status, &result.to_string());
                if let Some(gate) = state.read_gates.get(&key).cloned() {
                    response = response.gated(gate);
                }
                return response;
            } else if request.target.ends_with("/miotspec/prop/set") {
                let body: Value = serde_json::from_str(&request.body).unwrap();
                let items = body["params"].as_array().unwrap();
                let light = records.iter().any(|record| {
                    record.device.key.id == items[0]["did"].as_str().unwrap()
                        && record.device.functions[0].lighting.is_some()
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
                let mut response = MockResponse::json(status, &result.to_string());
                if let Some(gate) = state.property_gate.clone() {
                    response = response.gated(gate);
                }
                return response;
            } else if request.target.contains("get_token") {
                state.tokens += 1;
                if state.token_status != 200 {
                    return MockResponse::json(state.token_status, "{}");
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
                json!({"code":0,"result":{"cert":state.cert_pem.as_deref().unwrap_or(&certificate)}})
            } else if request.target.ends_with("/gethome") {
                state.homes += 1;
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
                    json!({"code":0,"result":{"homelist":[{"id":if state.other_home {"other"} else {"local"},"uid":if state.foreign {"foreign"}else {&records[0].uid},"roomlist":[],"dids":state.members}]}})
                }
            } else if request.target.ends_with("/device_list_page") {
                let body: Value = serde_json::from_str(&request.body).unwrap();
                let dids = body["dids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|did| did.as_str().unwrap().to_string())
                    .collect::<Vec<_>>();
                for did in &dids {
                    *state.detail_counts.entry(did.clone()).or_default() += 1;
                }
                let failed = dids
                    .iter()
                    .find_map(|did| state.detail_failures.get(did).copied());
                if dids.iter().all(|did| info_dids.contains(did)) {
                    state.infos += 1;
                    if let Some(status) = failed {
                        return MockResponse::json(status, "{}");
                    }
                    let value = json!({"code":0,"result":{"list":records.iter().filter(|record|state.members.contains(&record.device.key.id) && !state.missing_records.contains(&record.device.key.id)).map(|record|record_value(&state, record)).collect::<Vec<_>>(),"has_more":state.info_partial}});
                    let mut response = MockResponse::json(state.info_status, &value.to_string());
                    if let Some(gate) = state.info_gate.clone() {
                        response = response.gated(gate);
                    }
                    return response;
                }
                state.details += 1;
                if let Some(status) = failed {
                    let mut response = MockResponse::json(status, "{}")
                        .delayed(state.detail_delay)
                        .on_sent(move || detail_finished.lock().unwrap().detail_done += 1);
                    if let Some(gate) = state.detail_gate.clone() {
                        response = response.gated(gate);
                    }
                    return response;
                }
                if state.detail_status != 200 {
                    let mut response = MockResponse::json(state.detail_status, "{}")
                        .delayed(state.detail_delay)
                        .on_sent(move || detail_finished.lock().unwrap().detail_done += 1);
                    if let Some(gate) = state.detail_gate.clone() {
                        response = response.gated(gate);
                    }
                    return response;
                }
                json!({"code":0,"result":{"list":records.iter().filter(|record|state.members.contains(&record.device.key.id) && !state.missing_records.contains(&record.device.key.id)).map(|record|record_value(&state, record)).collect::<Vec<_>>()}})
            } else if request.target.starts_with("/miot-spec-v2/instance?") {
                let requested = request
                    .target
                    .split("type=")
                    .nth(1)
                    .and_then(|rest| rest.split('&').next())
                    .unwrap_or_default()
                    .replace("%3A", ":")
                    .replace("%3a", ":");
                let multi = request.target.contains("cuco-cp7pd");
                let light = records.iter().find(|record| {
                    record.instance_type == requested
                        && record
                            .device
                            .functions
                            .first()
                            .is_some_and(|function| function.lighting.is_some())
                });
                let mut value: Value = serde_json::from_str(if let Some(record) = light {
                    light_fixture(&record.model)
                } else if multi {
                    include_str!("../../../../../tests/fixtures/miot/cuco-cp7pd.json")
                } else {
                    include_str!("../../../../../tests/fixtures/miot/outlet.json")
                })
                .unwrap();
                if state
                    .unsupported
                    .contains(if multi { "multi" } else { "one" })
                {
                    value["services"] = json!([]);
                }
                value
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
                let body: Value = serde_json::from_str(&request.body).unwrap();
                let gate = body["dids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find_map(|did| state.detail_gates.get(did.as_str().unwrap()).cloned());
                let mut response = response.delayed(state.detail_delay);
                if let Some(gate) = gate {
                    response = response.gated(gate);
                }
                response
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
        let (mut source, ready) =
            XiaomiAdapter::build(store.clone(), client, discovery.clone(), clock.clone()).unwrap();
        source.test_ca = Some(tls.ca_pem.clone());
        source.request_timeout = Duration::from_millis(150);
        Self {
            _directory: directory,
            source,
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
        let keep_events = self.events.clone();
        let context = self.source.context.clone();
        let clock = self.clock.clone();
        let running = self.source.run(|_| {});
        let discovering = async {
            self.driver.run().await?;
            future::pending::<Result<(), AdapterError>>().await
        };
        let testing = test(Control {
            ready: Some(self.ready),
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
                future::or(future::or(running, discovering), async {
                    testing.await;
                    Ok(())
                }),
                async {
                    async_io::Timer::after(Duration::from_secs(5)).await;
                    panic!("source scenario stalled");
                },
            ),
        )
        .await;
        drop(keep_events);
        result
    }
}
impl Control {
    pub(in super::super) fn announce(&self) {
        self.events
            .send(announcement(
                self.listener.get_ref().local_addr().unwrap(),
                &self.data[0].uid,
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

fn record_value(state: &CloudState, record: &CachedPhysicalDevice) -> Value {
    let did = record.device.key.id.as_str();
    let mut value = json!({
        "did": did,
        "name": state.names.get(did).cloned().unwrap_or_else(|| format!("name {did}")),
        "model": record.model,
        "spec_type": record.instance_type,
        "isOnline": state.online.contains(did),
    });
    if let Some(group) = state.groups.get(did)
        && !group.missing
    {
        let member_ship = group
            .members
            .iter()
            .map(|member| (member.clone(), Value::String("1".into())))
            .collect::<serde_json::Map<_, _>>();
        value["extra"] = json!({
            "member_list": {
                "group_did": did,
                "status": "1",
                "member_ship": member_ship,
                "is_physical": group.physical,
            }
        });
    }
    value
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
