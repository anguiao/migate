use super::super::{Token, wire};
use serde_json::{Value, json};
use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc,
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
    pub(crate) hold_read: Arc<AtomicBool>,
    pub(crate) drop_business: Arc<AtomicBool>,
    pub(crate) reject_read: Arc<AtomicBool>,
    pub(crate) accept_replacement: Arc<AtomicBool>,
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
        let hold_read = Arc::new(AtomicBool::new(false));
        let holding = hold_read.clone();
        let drop_business = Arc::new(AtomicBool::new(false));
        let dropping = drop_business.clone();
        let reject_read = Arc::new(AtomicBool::new(false));
        let rejecting = reject_read.clone();
        let accept_replacement = Arc::new(AtomicBool::new(false));
        let accepting = accept_replacement.clone();
        let thread = thread::spawn(move || {
            let mut buffer = [0; 4096];
            while !stopped.load(Ordering::Relaxed) {
                let Ok((length, source)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                observed.fetch_add(1, Ordering::Relaxed);
                let packet = &buffer[..length];
                if packet == wire::HELLO {
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
                        let mut item = request["params"][0].clone();
                        item["code"] = json!(if rejecting.load(Ordering::Relaxed) {
                            -4004
                        } else {
                            0
                        });
                        item["value"] = json!(true);
                        json!([item])
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
            hold_read,
            drop_business,
            reject_read,
            accept_replacement,
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
