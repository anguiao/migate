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
    time::Instant,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceAccess {
    pub did: String,
    pub name: Option<String>,
    pub model: String,
    pub instance_type: String,
    pub online: bool,
    pub spec_v2_access: bool,
    pub push_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceEntry {
    pub did: String,
    pub access: Result<DeviceAccess, CentralError>,
}

pub(super) enum Response {
    Device(Option<DeviceAccess>),
    Devices(Vec<DeviceEntry>),
    Power(bool),
    Unit,
}
pub(super) type ResultValue = Result<Response, CentralError>;
struct Completion<T> {
    result: RefCell<Option<Result<T, CentralError>>>,
    event: Event,
}
pub(super) struct Request {
    pub(super) kind: RequestKind,
    pub(super) deadline: Instant,
    pub(super) complete: Box<dyn FnOnce(ResultValue)>,
    pub(super) id: Option<u32>,
}
pub(super) enum RequestKind {
    ListAll,
    List {
        did: String,
    },
    Subscribe {
        did: String,
        binding: PropertyBinding,
    },
    Unsubscribe {
        did: String,
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
            Self::ListAll => (
                "master/proxy/getDevList",
                json!({"info":["name","model","urn","online","specV2Access","pushAvailable"]}),
            ),
            Self::List { did } => (
                "master/proxy/getDevList",
                json!({"filter":{"did":[did]},"info":["name","model","urn","online","specV2Access","pushAvailable"]}),
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
            Self::Subscribe { .. } | Self::Unsubscribe { .. } => {
                unreachable!("subscriptions do not publish MIPS requests")
            }
        };
        Ok(Publish::new(
            topic,
            QoS::ExactlyOnce,
            wire::encode_mips(id, Some(reply_topic), &payload),
        ))
    }

    pub(super) fn operation(&self) -> &'static str {
        match self {
            Self::List { .. } | Self::ListAll => "getDevList",
            Self::Subscribe { .. } => "subscribe",
            Self::Unsubscribe { .. } => "unsubscribe",
            Self::Read { .. } => "get",
            Self::Write { .. } => "set",
        }
    }
    pub(super) fn did(&self) -> &str {
        match self {
            Self::ListAll => "*",
            Self::List { did }
            | Self::Subscribe { did, .. }
            | Self::Unsubscribe { did }
            | Self::Read { did, .. }
            | Self::Write { did, .. } => did,
        }
    }

    pub(super) fn cancelled_for(&self, did: &str) -> bool {
        !matches!(self, Self::ListAll | Self::Unsubscribe { .. }) && self.did() == did
    }
}
pub(super) struct Shared {
    pub(super) queue: RefCell<VecDeque<Request>>,
    pub(super) event: Event,
    pub(super) closed: Cell<bool>,
    pub(super) outstanding: Cell<usize>,
    pub(super) cancellations: RefCell<VecDeque<Cancellation>>,
}

pub(super) struct Cancellation {
    pub(super) did: String,
    pub(super) queued: VecDeque<Request>,
}

/// Requests are enqueued when these methods are called. Dropping the returned
/// future does not cancel an already sent write or suppress its result callback.
#[derive(Clone)]
pub struct SessionHandle {
    pub(super) shared: Rc<Shared>,
    pub(super) timing: Timing,
}
impl SessionHandle {
    pub fn close(&self) {
        self.shared.closed.set(true);
        self.shared.event.notify(usize::MAX);
    }
    pub fn get_devices<F: FnOnce(Result<Vec<DeviceEntry>, CentralError>) + 'static>(
        &self,
        on_result: F,
    ) -> impl Future<Output = Result<Vec<DeviceEntry>, CentralError>> + use<F> {
        self.submit(
            RequestKind::ListAll,
            |response| match response {
                Response::Devices(devices) => devices,
                _ => unreachable!("device list request returns devices"),
            },
            on_result,
        )
    }

    /// Cancel this device's existing work before the driver processes another frame.
    pub fn cancel_device(&self, did: &str) -> Result<(), CentralError> {
        if !valid_identifier(did) {
            return Err(CentralError::InvalidInput);
        }
        if self.shared.closed.get() {
            return Err(CentralError::Cancelled);
        }
        let mut queue = self.shared.queue.borrow_mut();
        let (queued, retained) = queue
            .drain(..)
            .partition(|request| request.kind.cancelled_for(did));
        *queue = retained;
        self.shared
            .cancellations
            .borrow_mut()
            .push_back(Cancellation {
                did: did.into(),
                queued,
            });
        self.shared.event.notify(usize::MAX);
        Ok(())
    }

    /// Also cancel pending subscriptions so a late SUBACK cannot restore routing.
    pub fn unsubscribe_device(
        &self,
        did: &str,
    ) -> impl Future<Output = Result<(), CentralError>> + use<> {
        let result = self
            .cancel_device(did)
            .map(|()| self.submit(RequestKind::Unsubscribe { did: did.into() }, |_| (), |_| {}));
        async move { result?.await }
    }
    pub fn get_device_list<F: FnOnce(Result<Option<DeviceAccess>, CentralError>) + 'static>(
        &self,
        did: &str,
        on_result: F,
    ) -> impl Future<Output = Result<Option<DeviceAccess>, CentralError>> + use<F> {
        self.submit(
            RequestKind::List { did: did.into() },
            |response| match response {
                Response::Device(device) => device,
                _ => unreachable!("filtered device list request returns a device"),
            },
            on_result,
        )
    }
    pub fn subscribe_property(
        &self,
        did: &str,
        binding: PropertyBinding,
    ) -> impl Future<Output = Result<(), CentralError>> + use<> {
        self.submit(
            RequestKind::Subscribe {
                did: did.into(),
                binding,
            },
            |_| (),
            |_| {},
        )
    }
    pub fn read_property<F: FnOnce(Result<bool, CentralError>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        on_result: F,
    ) -> impl Future<Output = Result<bool, CentralError>> + use<F> {
        self.submit(
            RequestKind::Read {
                did: did.into(),
                binding,
            },
            |response| match response {
                Response::Power(value) => value,
                _ => unreachable!("property read returns a power value"),
            },
            on_result,
        )
    }
    pub fn set_property<F: FnOnce(Result<(), CentralError>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        value: bool,
        on_result: F,
    ) -> impl Future<Output = Result<(), CentralError>> + use<F> {
        self.submit(
            RequestKind::Write {
                did: did.into(),
                binding,
                value,
            },
            |_| (),
            on_result,
        )
    }
    fn submit<T: Clone + 'static, F: FnOnce(Result<T, CentralError>) + 'static>(
        &self,
        kind: RequestKind,
        decode: fn(Response) -> T,
        on_result: F,
    ) -> impl Future<Output = Result<T, CentralError>> + use<T, F> {
        let completion = Rc::new(Completion {
            result: RefCell::new(None),
            event: Event::new(),
        });
        let request = Request {
            kind,
            deadline: Instant::now() + self.timing.request,
            complete: Box::new({
                let completion = completion.clone();
                move |result| {
                    let result = result.map(decode);
                    on_result(result.clone());
                    *completion.result.borrow_mut() = Some(result);
                    completion.event.notify(usize::MAX);
                }
            }),
            id: None,
        };
        let error = if !valid_request(&request.kind) {
            Some(CentralError::InvalidInput)
        } else if self.shared.closed.get() {
            Some(CentralError::Cancelled)
        } else if self.shared.outstanding.get() >= LIMIT {
            Some(CentralError::QueueFull)
        } else {
            None
        };
        if let Some(error) = error {
            (request.complete)(Err(error));
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
        RequestKind::ListAll => return true,
        RequestKind::List { did } | RequestKind::Unsubscribe { did } => (did, None),
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
        RequestKind::ListAll => {
            let list = value
                .get("devList")
                .and_then(Value::as_object)
                .ok_or(CentralError::Protocol)?;
            if list.keys().any(|did| !valid_identifier(did)) {
                return Err(CentralError::Protocol);
            }
            Ok(Response::Devices(
                list.iter()
                    .map(|(did, info)| DeviceEntry {
                        did: did.clone(),
                        access: device_access(did, info),
                    })
                    .collect(),
            ))
        }
        RequestKind::List { did } => {
            let list = value
                .get("devList")
                .and_then(Value::as_object)
                .ok_or(CentralError::Protocol)?;
            let Some(info) = list.get(did) else {
                return Ok(Response::Device(None));
            };
            Ok(Response::Device(Some(device_access(did, info)?)))
        }
        RequestKind::Subscribe { .. } | RequestKind::Unsubscribe { .. } => {
            Err(CentralError::Protocol)
        }
    }
}

fn device_access(did: &str, info: &Value) -> Result<DeviceAccess, CentralError> {
    if !valid_identifier(did) {
        return Err(CentralError::Protocol);
    }
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
    let name = match info.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name))
            if name.trim().is_empty() || name.chars().any(char::is_control) =>
        {
            None
        }
        Some(Value::String(name)) => Some(name.clone()),
        _ => return Err(CentralError::Protocol),
    };
    Ok(DeviceAccess {
        did: did.into(),
        name,
        model: text("model")?,
        instance_type: text("urn")?,
        online: flag("online")?,
        spec_v2_access: flag("specV2Access")?,
        push_available: flag("pushAvailable")?,
    })
}
