use super::super::{Token, wire};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub(crate) struct FakeDevice {
    pub(crate) address: SocketAddr,
    pub(crate) packets: Arc<AtomicUsize>,
    pub(crate) business: Arc<AtomicUsize>,
    pub(crate) reads: Arc<AtomicUsize>,
    pub(crate) writes: Arc<Mutex<Vec<Value>>>,
    pub(crate) values: Arc<Mutex<BTreeMap<(u64, u64), Value>>>,
    pub(crate) write_codes: Arc<Mutex<BTreeMap<(u64, u64), i64>>>,
    pub(crate) reverse_write_results: Arc<AtomicBool>,
    pub(crate) omit_write_result: Arc<AtomicBool>,
    pub(crate) hold_hello: Arc<AtomicBool>,
    pub(crate) hold_read: Arc<AtomicBool>,
    pub(crate) hold_write: Arc<AtomicBool>,
    pub(crate) drop_write: Arc<AtomicBool>,
    pub(crate) reject_write: Arc<AtomicBool>,
    pub(crate) drop_business: Arc<AtomicBool>,
    pub(crate) reject_read: Arc<AtomicBool>,
    pub(crate) accept_replacement: Arc<AtomicBool>,
    pub(crate) drop_replacement: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl FakeDevice {
    pub(crate) fn new(token: Token, replacement: Token) -> Self {
        Self::with_did(token, replacement, 656878122)
    }

    pub(crate) fn with_did(token: Token, replacement: Token, did: u64) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let packets = Arc::new(AtomicUsize::new(0));
        let observed = packets.clone();
        let business = Arc::new(AtomicUsize::new(0));
        let business_count = business.clone();
        let reads = Arc::new(AtomicUsize::new(0));
        let read_count = reads.clone();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let observed_writes = writes.clone();
        let values = Arc::new(Mutex::new(BTreeMap::from([
            ((2, 1), json!(true)),
            ((2, 3), json!(1)),
            ((2, 4), json!(false)),
            ((8, 1), json!(40)),
        ])));
        let device_values = values.clone();
        let write_codes = Arc::new(Mutex::new(BTreeMap::new()));
        let device_codes = write_codes.clone();
        let reverse_write_results = Arc::new(AtomicBool::new(false));
        let reverse_results = reverse_write_results.clone();
        let omit_write_result = Arc::new(AtomicBool::new(false));
        let omit_result = omit_write_result.clone();
        let hold_hello = Arc::new(AtomicBool::new(false));
        let holding_hello = hold_hello.clone();
        let hold_read = Arc::new(AtomicBool::new(false));
        let holding = hold_read.clone();
        let hold_write = Arc::new(AtomicBool::new(false));
        let holding_write = hold_write.clone();
        let drop_write = Arc::new(AtomicBool::new(false));
        let dropping_write = drop_write.clone();
        let reject_write = Arc::new(AtomicBool::new(false));
        let rejecting_write = reject_write.clone();
        let drop_business = Arc::new(AtomicBool::new(false));
        let dropping = drop_business.clone();
        let reject_read = Arc::new(AtomicBool::new(false));
        let rejecting = reject_read.clone();
        let accept_replacement = Arc::new(AtomicBool::new(false));
        let accepting = accept_replacement.clone();
        let drop_replacement = Arc::new(AtomicBool::new(false));
        let dropping_replacement = drop_replacement.clone();
        let thread = thread::spawn(move || {
            let mut buffer = [0; 4096];
            while !stopped.load(Ordering::Relaxed) {
                let Ok((length, source)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                observed.fetch_add(1, Ordering::Relaxed);
                let packet = &buffer[..length];
                if packet == wire::HELLO {
                    while holding_hello.load(Ordering::Relaxed) && !stopped.load(Ordering::Relaxed)
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut hello = wire::HELLO;
                    hello[4..12].copy_from_slice(&did.to_be_bytes());
                    hello[12..16].copy_from_slice(&42_u32.to_be_bytes());
                    socket.send_to(&hello, source).unwrap();
                    continue;
                }
                business_count.fetch_add(1, Ordering::Relaxed);
                let (decoded, used, model) = if let Some(decoded) = wire::decode(packet, &token) {
                    (decoded, &token, "dmaker.fan.p5c")
                } else if let Some(decoded) = wire::decode(packet, &replacement) {
                    if dropping_replacement.load(Ordering::Relaxed) {
                        continue;
                    }
                    (
                        decoded,
                        &replacement,
                        if accepting.load(Ordering::Relaxed) {
                            "dmaker.fan.p5c"
                        } else {
                            "other.model"
                        },
                    )
                } else {
                    continue;
                };
                if dropping.load(Ordering::Relaxed) {
                    continue;
                }
                let request: Value = serde_json::from_slice(&decoded.2).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "miIO.info" => json!({"model":model}),
                    "get_properties" => {
                        read_count.fetch_add(1, Ordering::Relaxed);
                        while holding.load(Ordering::Relaxed) && !stopped.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(1));
                        }
                        if stopped.load(Ordering::Relaxed) {
                            break;
                        }
                        let values = device_values.lock().unwrap();
                        Value::Array(
                            request["params"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|item| {
                                    let mut item = item.clone();
                                    item["code"] = json!(if rejecting.load(Ordering::Relaxed) {
                                        -4004
                                    } else {
                                        0
                                    });
                                    item["value"] = values[&(
                                        item["siid"].as_u64().unwrap(),
                                        item["piid"].as_u64().unwrap(),
                                    )]
                                        .clone();
                                    item
                                })
                                .collect(),
                        )
                    }
                    "set_properties" => {
                        observed_writes
                            .lock()
                            .unwrap()
                            .extend(request["params"].as_array().unwrap().iter().cloned());
                        while holding_write.load(Ordering::Relaxed)
                            && !stopped.load(Ordering::Relaxed)
                        {
                            thread::sleep(Duration::from_millis(1));
                        }
                        if stopped.load(Ordering::Relaxed) {
                            break;
                        }
                        if dropping_write.load(Ordering::Relaxed) {
                            continue;
                        }
                        let rejected = rejecting_write.load(Ordering::Relaxed);
                        let mut values = device_values.lock().unwrap();
                        let codes = device_codes.lock().unwrap();
                        let mut results = request["params"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|item| {
                                let binding = (
                                    item["siid"].as_u64().unwrap(),
                                    item["piid"].as_u64().unwrap(),
                                );
                                let code = if rejected {
                                    -4004
                                } else {
                                    *codes.get(&binding).unwrap_or(&0)
                                };
                                if code == 0 {
                                    values.insert(binding, item["value"].clone());
                                }
                                let mut item = item.clone();
                                item["code"] = json!(code);
                                item
                            })
                            .collect::<Vec<_>>();
                        if reverse_results.load(Ordering::Relaxed) {
                            results.reverse();
                        }
                        if omit_result.load(Ordering::Relaxed) {
                            results.pop();
                        }
                        Value::Array(results)
                    }
                    method => panic!("unexpected method {method}"),
                };
                let response = json!({"id":request["id"],"result":result});
                let packet =
                    wire::encode(decoded.0, 43, used, &serde_json::to_vec(&response).unwrap());
                socket.send_to(&packet, source).unwrap();
            }
        });
        Self {
            address,
            packets,
            business,
            reads,
            writes,
            values,
            write_codes,
            reverse_write_results,
            omit_write_result,
            hold_hello,
            hold_read,
            hold_write,
            drop_write,
            reject_write,
            drop_business,
            reject_read,
            accept_replacement,
            drop_replacement,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for FakeDevice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

pub(crate) struct LegacyDevice {
    pub(crate) address: SocketAddr,
    pub(crate) requests: Arc<Mutex<Vec<Value>>>,
    pub(crate) values: Arc<Mutex<Vec<Value>>>,
    pub(crate) ack: Arc<Mutex<Value>>,
    pub(crate) rejected: Arc<Mutex<BTreeMap<String, i64>>>,
    pub(crate) drop_write: Arc<AtomicBool>,
    pub(crate) hold_read: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LegacyDevice {
    pub(crate) fn new(token: Token, did: u64) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let observed = requests.clone();
        let values = Arc::new(Mutex::new(vec![
            json!("off"),
            json!("cool"),
            json!(24),
            json!("auto_fan"),
            json!("off"),
        ]));
        let feedback = values.clone();
        let ack = Arc::new(Mutex::new(json!(["ok"])));
        let reply = ack.clone();
        let rejected = Arc::new(Mutex::new(BTreeMap::<String, i64>::new()));
        let rejections = rejected.clone();
        let drop_write = Arc::new(AtomicBool::new(false));
        let dropping = drop_write.clone();
        let hold_read = Arc::new(AtomicBool::new(false));
        let holding = hold_read.clone();
        let thread = thread::spawn(move || {
            let mut buffer = [0; 4096];
            let mut held_reads = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                if !holding.load(Ordering::Relaxed) {
                    for (did, source, id) in held_reads.drain(..) {
                        let response = json!({"id":id,"result":*feedback.lock().unwrap()});
                        let packet =
                            wire::encode(did, 43, &token, &serde_json::to_vec(&response).unwrap());
                        socket.send_to(&packet, source).unwrap();
                    }
                }
                let Ok((length, source)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                let packet = &buffer[..length];
                if packet == wire::HELLO {
                    let mut hello = wire::HELLO;
                    hello[4..12].copy_from_slice(&did.to_be_bytes());
                    hello[12..16].copy_from_slice(&42_u32.to_be_bytes());
                    socket.send_to(&hello, source).unwrap();
                    continue;
                }
                let Some(decoded) = wire::decode(packet, &token) else {
                    continue;
                };
                let request: Value = serde_json::from_slice(&decoded.2).unwrap();
                observed.lock().unwrap().push(request.clone());
                let method = request["method"].as_str().unwrap();
                if method == "get_prop" && holding.load(Ordering::Relaxed) {
                    held_reads.push((decoded.0, source, request["id"].clone()));
                    continue;
                }
                let response = if let Some(code) = rejections.lock().unwrap().get(method).copied() {
                    json!({"id":request["id"], "error":{"code":code,"message":"rejected"}})
                } else {
                    let result = match method {
                        "miIO.info" => json!({"model":"lumi.acpartner.mcn02"}),
                        "get_prop" => json!(*feedback.lock().unwrap()),
                        "set_power" | "set_mode" | "set_tar_temp" | "set_fan_level"
                        | "set_ver_swing" => {
                            if dropping.load(Ordering::Relaxed) {
                                continue;
                            }
                            let index = match method {
                                "set_power" => 0,
                                "set_mode" => 1,
                                "set_tar_temp" => 2,
                                "set_fan_level" => 3,
                                "set_ver_swing" => 4,
                                _ => unreachable!(),
                            };
                            feedback.lock().unwrap()[index] = request["params"][0].clone();
                            reply.lock().unwrap().clone()
                        }
                        _ => panic!("unexpected method {method}"),
                    };
                    json!({"id":request["id"],"result":result})
                };
                let packet = wire::encode(
                    decoded.0,
                    43,
                    &token,
                    &serde_json::to_vec(&response).unwrap(),
                );
                socket.send_to(&packet, source).unwrap();
            }
        });
        Self {
            address,
            requests,
            values,
            ack,
            rejected,
            drop_write,
            hold_read,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for LegacyDevice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
