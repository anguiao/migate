use super::*;

pub(in super::super) type Binding = (String, u32, u32);
#[derive(Default)]
pub(in super::super) struct PeerState {
    pub(in super::super) client_id: String,
    pub(in super::super) seen: Vec<(String, Value)>,
    pub(in super::super) names: BTreeMap<String, String>,
    pub(in super::super) models: BTreeMap<String, String>,
    pub(in super::super) online: BTreeMap<String, bool>,
    pub(in super::super) bad: BTreeSet<String>,
    pub(in super::super) pushless: BTreeSet<String>,
    pub(in super::super) omit: BTreeSet<String>,
    pub(in super::super) values: BTreeMap<Binding, bool>,
    pub(in super::super) numeric_values: BTreeMap<Binding, Value>,
    pub(in super::super) update_times: BTreeMap<Binding, i64>,
    pub(in super::super) sub_reject: BTreeSet<Binding>,
    pub(in super::super) sub_timeout: BTreeSet<Binding>,
    pub(in super::super) held_subs: Vec<Subscribe>,
    pub(in super::super) read_fail: BTreeSet<Binding>,
    pub(in super::super) read_timeout: BTreeSet<Binding>,
    pub(in super::super) list_error: bool,
    pub(in super::super) list_timeout: bool,
    pub(in super::super) hold_reads: bool,
    pub(in super::super) hold_writes: bool,
    pub(in super::super) held: Vec<(Publish, Value)>,
    pub(in super::super) send: Vec<Packet>,
    pub(in super::super) unsubscribes: Vec<String>,
}
#[derive(Default)]
pub(in super::super) struct Peer {
    pub(in super::super) state: RefCell<PeerState>,
    pub(in super::super) changed: Event,
}
impl Peer {
    pub(in super::super) fn value(
        &self,
        did: &str,
        binding: crate::xiaomi::capability::PropertyBinding,
        value: Value,
    ) {
        self.state
            .borrow_mut()
            .numeric_values
            .insert((did.into(), binding.siid, binding.piid), value);
    }
    pub(in super::super) fn notify_value(
        &self,
        did: &str,
        binding: crate::xiaomi::capability::PropertyBinding,
        value: Value,
        retain: bool,
    ) {
        self.notify_value_at(did, binding, value, None, retain);
    }

    pub(in super::super) fn notify_value_at(
        &self,
        did: &str,
        binding: crate::xiaomi::capability::PropertyBinding,
        value: Value,
        update_time: Option<i64>,
        retain: bool,
    ) {
        let client = self.state.borrow().client_id.clone();
        let mut payload = json!({"did":did,"siid":binding.siid,"piid":binding.piid,"value":value});
        if let Some(update_time) = update_time {
            payload["updateTime"] = json!(update_time);
        }
        let mut publish = Publish::new(
            format!(
                "{client}/appMsg/notify/iot/{did}/property/{}.{}",
                binding.siid, binding.piid
            ),
            QoS::AtMostOnce,
            encode_mips(700, None, &payload),
        );
        publish.retain = retain;
        self.send(Packet::Publish(publish));
    }

    pub(in super::super) fn lists(&self) -> usize {
        self.state
            .borrow()
            .seen
            .iter()
            .filter(|(operation, _)| operation == "getDevList")
            .count()
    }
    pub(in super::super) fn notify(&self, did: &str, siid: u32, value: bool) {
        let client = self.state.borrow().client_id.clone();
        self.send(Packet::Publish(Publish::new(
            format!("{client}/appMsg/notify/iot/{did}/property/{siid}.1"),
            QoS::AtMostOnce,
            encode_mips(
                700,
                None,
                &json!({"did":did,"siid":siid,"piid":1,"value":value}),
            ),
        )));
    }
    pub(in super::super) fn list_changed(&self, dids: &[&str]) {
        let client = self.state.borrow().client_id.clone();
        self.send(Packet::Publish(Publish::new(
            format!("{client}/appMsg/devListChange"),
            QoS::AtMostOnce,
            encode_mips(701, None, &json!({"devList":dids})),
        )));
    }
    pub(in super::super) fn send(&self, packet: Packet) {
        self.state.borrow_mut().send.push(packet);
        self.changed.notify(usize::MAX);
    }
    pub(in super::super) async fn release_property(
        &self,
        did: &str,
        binding: crate::xiaomi::capability::PropertyBinding,
        value: Value,
    ) {
        let matches = |publish: &Publish| {
            let payload = decode_mips(&publish.payload).unwrap().payload;
            payload["did"] == did
                && (payload["siid"] == binding.siid && payload["piid"] == binding.piid
                    || payload["rpc"]["params"].as_array().is_some_and(|items| {
                        items.iter().any(|item| {
                            item["siid"] == binding.siid && item["piid"] == binding.piid
                        })
                    }))
        };
        until(|| {
            self.state
                .borrow()
                .held
                .iter()
                .any(|(publish, _)| matches(publish))
        })
        .await;
        let index = self
            .state
            .borrow()
            .held
            .iter()
            .position(|(publish, _)| matches(publish))
            .unwrap();
        let (publish, _) = self.state.borrow_mut().held.remove(index);
        let client = self.state.borrow().client_id.clone();
        self.send(response(&client, &publish, value));
    }
    pub(in super::super) async fn release(&self, did: &str, siid: u32, value: Value) {
        until(|| {
            self.state.borrow().held.iter().any(|(publish, _)| {
                let payload = decode_mips(&publish.payload).unwrap().payload;
                payload["did"] == did
                    && (payload["siid"] == siid || payload["rpc"]["params"][0]["siid"] == siid)
            })
        })
        .await;
        let index = self
            .state
            .borrow()
            .held
            .iter()
            .position(|(publish, _)| {
                let payload = decode_mips(&publish.payload).unwrap().payload;
                payload["did"] == did
                    && (payload["siid"] == siid || payload["rpc"]["params"][0]["siid"] == siid)
            })
            .unwrap();
        let (publish, _) = self.state.borrow_mut().held.remove(index);
        let client = self.state.borrow().client_id.clone();
        self.send(response(&client, &publish, value));
    }
    pub(in super::super) fn count(&self, operation: &str, did: &str, siid: u32) -> usize {
        self.state
            .borrow()
            .seen
            .iter()
            .filter(|(op, value)| {
                op == operation
                    && value["did"] == did
                    && (siid == 0
                        || value["siid"] == siid
                        || value["rpc"]["params"][0]["siid"] == siid)
            })
            .count()
    }
}
pub(in super::super) fn response(client: &str, publish: &Publish, value: Value) -> Packet {
    Packet::Publish(Publish::new(
        format!("{client}/reply"),
        QoS::AtMostOnce,
        encode_mips(decode_mips(&publish.payload).unwrap().id, None, &value),
    ))
}
pub(in super::super) fn key(value: &Value) -> Binding {
    (
        value["did"].as_str().unwrap().into(),
        value["siid"].as_u64().unwrap() as u32,
        value["piid"].as_u64().unwrap() as u32,
    )
}
pub(in super::super) fn topic_key(topic: &str) -> Binding {
    let parts = topic.split('/').collect::<Vec<_>>();
    let binding = parts.last().unwrap().split('.').collect::<Vec<_>>();
    (
        parts[parts.len() - 3].into(),
        binding[0].parse().unwrap(),
        binding[1].parse().unwrap(),
    )
}
pub(in super::super) async fn peer_run(
    mut broker: Broker,
    peer: Rc<Peer>,
    data: Vec<NativeDescription>,
) {
    loop {
        let listener = peer.changed.listen();
        let queued = std::mem::take(&mut peer.state.borrow_mut().send);
        if !queued.is_empty() {
            broker.batch(queued).await;
            continue;
        }
        let packet = future::or(async { Some(broker.next().await) }, async {
            listener.await;
            None
        })
        .await;
        let Some(packet) = packet else {
            continue;
        };
        match packet {
            Packet::Subscribe(sub) => {
                let binding = topic_key(&sub.filters[0].path);
                let (timeout, rejected) = {
                    let mut state = peer.state.borrow_mut();
                    state.seen.push((
                        "subscribe".into(),
                        json!({"did":binding.0,"siid":binding.1,"piid":binding.2}),
                    ));
                    let timeout = state.sub_timeout.contains(&binding);
                    let rejected = state.sub_reject.contains(&binding);
                    if timeout {
                        state.held_subs.push(sub.clone());
                    }
                    (timeout, rejected)
                };
                if !timeout {
                    broker
                        .send(Packet::SubAck(SubAck::new(
                            sub.pkid,
                            vec![if rejected {
                                SubscribeReasonCode::Unspecified
                            } else {
                                SubscribeReasonCode::QoS2
                            }],
                        )))
                        .await;
                }
            }
            Packet::Unsubscribe(unsub) => {
                peer.state
                    .borrow_mut()
                    .unsubscribes
                    .extend(unsub.filters.clone());
                broker
                    .send(Packet::UnsubAck(UnsubAck {
                        pkid: unsub.pkid,
                        reasons: vec![UnsubAckReason::Success; unsub.filters.len()],
                        properties: None,
                    }))
                    .await;
            }
            Packet::Publish(publish) => {
                let payload = decode_mips(&publish.payload).unwrap().payload;
                let operation = publish.topic.rsplit('/').next().unwrap().to_owned();
                peer.state
                    .borrow_mut()
                    .seen
                    .push((operation.clone(), payload.clone()));
                broker.acknowledge(&publish).await;
                let (reply, hold, timeout) = {
                    let state = peer.state.borrow();
                    match operation.as_str() {
                        "getDevList" => {
                            let mut list = serde_json::Map::new();
                            for record in &data {
                                let did = &record.device().key().id;
                                if state.omit.contains(did)
                                    || payload["filter"]["did"].as_array().is_some_and(|dids| {
                                        !dids.iter().any(|value| value.as_str() == Some(did))
                                    })
                                {
                                    continue;
                                }
                                let mut info = json!({"name":state.names.get(did).cloned().unwrap_or_else(||format!("name {did}")),"model":state.models.get(did).map(String::as_str).unwrap_or(record.model()),"urn":record.instance_type(),"online":state.online.get(did).copied().unwrap_or(true),"specV2Access":true,"pushAvailable":!state.pushless.contains(did)});
                                if state.bad.contains(did) {
                                    info.as_object_mut().unwrap().remove("pushAvailable");
                                }
                                list.insert(did.clone(), info);
                            }
                            (
                                if state.list_error {
                                    json!({"invalid":list})
                                } else {
                                    json!({"devList":list})
                                },
                                false,
                                state.list_timeout,
                            )
                        }
                        "get" => {
                            let binding = key(&payload);
                            (
                                if state.read_fail.contains(&binding) {
                                    json!({"code":-7})
                                } else {
                                    let mut value = json!({"value":state.numeric_values.get(&binding).cloned().unwrap_or_else(||json!(state.values.get(&binding).copied().unwrap_or(false)))});
                                    if let Some(update_time) = state.update_times.get(&binding) {
                                        value["updateTime"] = json!(update_time);
                                    }
                                    value
                                },
                                state.hold_reads,
                                state.read_timeout.contains(&binding),
                            )
                        }
                        "rpcReq" => {
                            let properties = payload["rpc"]["params"].as_array().unwrap();
                            (
                                json!({"result": properties.iter().map(|property| json!({"did":property["did"],"siid":property["siid"],"piid":property["piid"],"code":0})).collect::<Vec<_>>()}),
                                state.hold_writes,
                                false,
                            )
                        }
                        _ => panic!("unexpected operation {operation}"),
                    }
                };
                if hold || timeout {
                    peer.state.borrow_mut().held.push((publish, reply));
                } else {
                    broker
                        .send(response(&broker.client_id, &publish, reply))
                        .await;
                }
            }
            other => panic!("unexpected source packet {other:?}"),
        }
    }
}
