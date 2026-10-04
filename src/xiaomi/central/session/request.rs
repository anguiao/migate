use super::super::{CentralError, wire};
use super::{LIMIT, Timing};
use crate::xiaomi::capability::PropertyBinding;
use event_listener::Event;
use mqttbytes::{QoS, v5::Publish};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    future::Future,
    rc::Rc,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceAccess {
    pub did: String,
    pub model: String,
    pub instance_type: String,
    pub online: bool,
    pub spec_v2_access: bool,
    pub push_available: bool,
}

#[derive(Clone)]
pub(super) enum Response {
    Device(Option<DeviceAccess>),
    Power(bool),
    Unit,
}
pub(super) type ResultValue = Result<Response, CentralError>;
type Callback = Box<dyn FnOnce(ResultValue)>;
struct Completion {
    result: RefCell<Option<ResultValue>>,
    event: Event,
}
pub(super) struct Request {
    pub(super) kind: RequestKind,
    pub(super) deadline: Instant,
    completion: Rc<Completion>,
    callback: Option<Callback>,
    pub(super) id: Option<u32>,
}
#[derive(Clone)]
pub(super) enum RequestKind {
    List {
        did: String,
    },
    Subscribe {
        did: String,
        binding: PropertyBinding,
    },
    Read {
        did: String,
        binding: PropertyBinding,
    },
    Write {
        did: String,
        binding: PropertyBinding,
        value: bool,
    },
}
impl RequestKind {
    pub(super) fn publication(&self, id: u32, reply_topic: &str) -> Result<Publish, CentralError> {
        let (topic, payload) = match self {
            Self::List { did } => (
                "master/proxy/getDevList",
                json!({"filter":{"did":[did]},"info":["model","urn","online","specV2Access","pushAvailable"]}),
            ),
            Self::Read { did, binding } => (
                "master/proxy/get",
                json!({"did":did,"siid":binding.siid,"piid":binding.piid}),
            ),
            Self::Write {
                did,
                binding,
                value,
            } => (
                "master/proxy/rpcReq",
                json!({"did":did,"rpc":{"id":next_mips_id()?,"method":"set_properties","params":[{"did":did,"siid":binding.siid,"piid":binding.piid,"value":value}]}}),
            ),
            Self::Subscribe { .. } => unreachable!("subscriptions do not publish MIPS requests"),
        };
        Ok(Publish::new(
            topic,
            QoS::ExactlyOnce,
            wire::encode_mips(id, Some(reply_topic), &payload),
        ))
    }

    pub(super) fn operation(&self) -> &'static str {
        match self {
            Self::List { .. } => "getDevList",
            Self::Subscribe { .. } => "subscribe",
            Self::Read { .. } => "get",
            Self::Write { .. } => "set",
        }
    }
    pub(super) fn did(&self) -> &str {
        match self {
            Self::List { did }
            | Self::Subscribe { did, .. }
            | Self::Read { did, .. }
            | Self::Write { did, .. } => did,
        }
    }
}
impl Request {
    pub(super) fn finish(mut self, result: ResultValue) {
        if let Some(callback) = self.callback.take() {
            callback(result.clone());
        }
        *self.completion.result.borrow_mut() = Some(result);
        self.completion.event.notify(usize::MAX);
    }
}
pub(super) struct Shared {
    pub(super) queue: RefCell<VecDeque<Request>>,
    pub(super) event: Event,
    pub(super) closed: Cell<bool>,
    pub(super) outstanding: Cell<usize>,
    pub(super) generation: u64,
}

/// Requests are enqueued when these methods are called. Dropping the returned
/// future does not cancel an already sent write or suppress its result callback.
#[derive(Clone)]
pub struct SessionHandle {
    pub(super) shared: Rc<Shared>,
    pub(super) timing: Timing,
}
impl SessionHandle {
    pub fn generation(&self) -> u64 {
        self.shared.generation
    }
    pub fn close(&self) {
        self.shared.closed.set(true);
        self.shared.event.notify(usize::MAX);
    }
    pub fn get_device_list<F: FnOnce(Result<Option<DeviceAccess>, CentralError>) + 'static>(
        &self,
        did: &str,
        on_result: F,
    ) -> impl Future<Output = Result<Option<DeviceAccess>, CentralError>> + use<F> {
        let callback: Callback = Box::new(move |result| {
            on_result(result.and_then(|response| match response {
                Response::Device(device) => Ok(device),
                _ => Err(CentralError::Protocol),
            }))
        });
        let result = self.submit(
            RequestKind::List { did: did.into() },
            self.timing.request,
            Some(callback),
        );
        async move {
            match result.await? {
                Response::Device(device) => Ok(device),
                _ => Err(CentralError::Protocol),
            }
        }
    }
    pub fn subscribe_property(
        &self,
        did: &str,
        binding: PropertyBinding,
    ) -> impl Future<Output = Result<(), CentralError>> + use<> {
        let result = self.submit(
            RequestKind::Subscribe {
                did: did.into(),
                binding,
            },
            self.timing.request,
            None,
        );
        async move { result.await.map(|_| ()) }
    }
    pub fn read_property<F: FnOnce(Result<bool, CentralError>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        on_result: F,
    ) -> impl Future<Output = Result<bool, CentralError>> + use<F> {
        let callback: Callback = Box::new(move |result| {
            on_result(result.and_then(|response| match response {
                Response::Power(value) => Ok(value),
                _ => Err(CentralError::Protocol),
            }))
        });
        let result = self.submit(
            RequestKind::Read {
                did: did.into(),
                binding,
            },
            self.timing.request,
            Some(callback),
        );
        async move {
            match result.await? {
                Response::Power(value) => Ok(value),
                _ => Err(CentralError::Protocol),
            }
        }
    }
    pub fn set_property<F: FnOnce(Result<(), CentralError>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        value: bool,
        remaining: Duration,
        on_result: F,
    ) -> impl Future<Output = Result<(), CentralError>> + use<F> {
        let callback: Callback = Box::new(move |result| on_result(result.map(|_| ())));
        let result = self.submit(
            RequestKind::Write {
                did: did.into(),
                binding,
                value,
            },
            remaining,
            Some(callback),
        );
        async move { result.await.map(|_| ()) }
    }
    fn submit(
        &self,
        kind: RequestKind,
        remaining: Duration,
        callback: Option<Callback>,
    ) -> impl Future<Output = ResultValue> + use<> {
        let completion = Rc::new(Completion {
            result: RefCell::new(None),
            event: Event::new(),
        });
        let now = Instant::now();
        let deadline = now.checked_add(remaining);
        let request = Request {
            kind,
            deadline: deadline.unwrap_or(now),
            completion: completion.clone(),
            callback,
            id: None,
        };
        let error = if deadline.is_none() || !valid_request(&request.kind) {
            Some(CentralError::InvalidInput)
        } else if remaining.is_zero() {
            Some(CentralError::Timeout)
        } else if self.shared.closed.get() {
            Some(CentralError::Cancelled)
        } else if self.shared.outstanding.get() >= LIMIT {
            Some(CentralError::QueueFull)
        } else {
            None
        };
        if let Some(error) = error {
            request.finish(Err(error));
        } else {
            self.shared
                .outstanding
                .set(self.shared.outstanding.get() + 1);
            self.shared.queue.borrow_mut().push_back(request);
            self.shared.event.notify(usize::MAX);
        }
        async move {
            loop {
                let listener = completion.event.listen();
                if let Some(result) = completion.result.borrow_mut().take() {
                    return result;
                }
                listener.await;
            }
        }
    }
}

pub(super) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}
fn valid_request(kind: &RequestKind) -> bool {
    let (did, binding) = match kind {
        RequestKind::List { did } => (did, None),
        RequestKind::Subscribe { did, binding }
        | RequestKind::Read { did, binding }
        | RequestKind::Write { did, binding, .. } => (did, Some(binding)),
    };
    valid_identifier(did) && binding.is_none_or(|binding| binding.siid > 0 && binding.piid > 0)
}
pub(super) fn next_mips_id() -> Result<u32, CentralError> {
    static IDS: OnceLock<AtomicU64> = OnceLock::new();
    let next = IDS
        .get_or_init(|| AtomicU64::new(u64::from(rand::random::<u32>() >> 1)))
        .fetch_add(1, Ordering::Relaxed);
    u32::try_from(next).map_err(|_| CentralError::Protocol)
}
pub(super) fn property_topic(prefix: &str, did: &str, binding: PropertyBinding) -> String {
    format!(
        "{prefix}/appMsg/notify/iot/{did}/property/{}.{}",
        binding.siid, binding.piid
    )
}

pub(super) fn validate_identity(
    value: &Value,
    did: &str,
    binding: PropertyBinding,
    required: bool,
) -> Result<(), CentralError> {
    for (name, expected) in [("siid", binding.siid), ("piid", binding.piid)] {
        if (required || value.get(name).is_some())
            && value.get(name).and_then(Value::as_u64) != Some(u64::from(expected))
        {
            return Err(CentralError::Protocol);
        }
    }
    if (required || value.get("did").is_some())
        && value.get("did").and_then(Value::as_str) != Some(did)
    {
        return Err(CentralError::Protocol);
    }
    Ok(())
}
pub(super) fn parse_response(kind: &RequestKind, value: &Value) -> ResultValue {
    if let Some(error) = value.get("error") {
        return Err(CentralError::Rejected(
            error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or(CentralError::Protocol)?,
        ));
    }
    match kind {
        RequestKind::Read { did, binding } => {
            validate_identity(value, did, *binding, false)?;
            Ok(Response::Power(
                value
                    .get("value")
                    .and_then(Value::as_bool)
                    .ok_or(CentralError::Protocol)?,
            ))
        }
        RequestKind::Write { did, binding, .. } => {
            let result = value
                .get("result")
                .and_then(Value::as_array)
                .filter(|result| result.len() == 1)
                .ok_or(CentralError::Protocol)?;
            let result = &result[0];
            if result.get("did").and_then(Value::as_str) != Some(did) {
                return Err(CentralError::Protocol);
            }
            validate_identity(result, did, *binding, false)?;
            match result
                .get("code")
                .and_then(Value::as_i64)
                .ok_or(CentralError::Protocol)?
            {
                0 | 1 => Ok(Response::Unit),
                code => Err(CentralError::Rejected(code)),
            }
        }
        RequestKind::List { did } => {
            let list = value
                .get("devList")
                .and_then(Value::as_object)
                .ok_or(CentralError::Protocol)?;
            let Some(info) = list.get(did) else {
                return Ok(Response::Device(None));
            };
            let text = |key| {
                info.get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or(CentralError::Protocol)
            };
            let flag = |key| {
                info.get(key)
                    .and_then(Value::as_bool)
                    .ok_or(CentralError::Protocol)
            };
            Ok(Response::Device(Some(DeviceAccess {
                did: did.clone(),
                model: text("model")?,
                instance_type: text("urn")?,
                online: flag("online")?,
                spec_v2_access: flag("specV2Access")?,
                push_available: flag("pushAvailable")?,
            })))
        }
        RequestKind::Subscribe { .. } => Err(CentralError::Protocol),
    }
}
