use super::super::{CentralError, wire};
use super::{LIMIT, Timing};
use crate::xiaomi::{
    capability::{EventBinding, PropertyBinding},
    property::{
        PropertyReading, PropertyResults, correlate, update_time, valid_bindings, valid_value,
    },
};
use event_listener::Event;
use mqttbytes::{QoS, v5::Publish};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, VecDeque},
    future::Future,
    rc::Rc,
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
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

/// The request's observed progress, independent of its completion error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestStage {
    /// No request bytes have been accepted by the socket.
    NotSent,
    /// At least one request byte was accepted; remote execution is uncertain.
    MayHaveBeenSent,
    /// A valid, correlated business response reported acceptance or rejection.
    BusinessResult,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestTimeout {
    /// The operation deadline elapsed before any request bytes were written.
    Queue,
    /// A sent business request did not receive a valid response in time.
    Response,
    /// A subscription or unsubscription confirmation did not arrive in time.
    Subscription,
    /// MQTT, socket output, or keepalive timed out and ended the session.
    Transport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestOutcome<T> {
    pub result: Result<T, CentralError>,
    pub stage: RequestStage,
    pub timeout: Option<RequestTimeout>,
}

/// A last check made while the operation has written no socket bytes.
pub enum SendDecision {
    Send,
    Withdraw,
    Cancel,
}

pub struct SendHooks {
    pub(super) guard: Box<dyn FnMut() -> SendDecision>,
    pub(super) first_send: Box<dyn FnOnce()>,
}
impl SendHooks {
    pub fn new(
        guard: impl FnMut() -> SendDecision + 'static,
        on_first_send: impl FnOnce() + 'static,
    ) -> Self {
        Self {
            guard: Box::new(guard),
            first_send: Box::new(on_first_send),
        }
    }
}

pub(super) enum Response {
    Devices(Vec<DeviceEntry>),
    PropertyValue(PropertyReading),
    Properties(PropertyResults<(), CentralError>),
    Unit,
}
pub(super) type ResultValue = Result<Response, CentralError>;
struct Completion<T> {
    result: RefCell<Option<RequestOutcome<T>>>,
    event: Event,
}
pub(super) struct Request {
    pub(super) kind: RequestKind,
    pub(super) deadline: Instant,
    pub(super) complete: Box<dyn FnOnce(ResultValue, RequestStage, Option<RequestTimeout>)>,
    pub(super) id: Option<u32>,
    pub(super) stage: RequestStage,
    pub(super) hooks: Option<SendHooks>,
}
pub(super) enum RequestKind {
    ListAll,
    ListFiltered {
        dids: Vec<String>,
    },
    Subscribe {
        did: String,
        binding: NotificationBinding,
    },
    Unsubscribe {
        did: String,
        scope: CancellationScope,
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
    WriteProperties {
        did: String,
        properties: Vec<(PropertyBinding, Value)>,
    },
}
impl RequestKind {
    pub(super) fn is_read_or_list(&self) -> bool {
        matches!(
            self,
            Self::ListAll | Self::ListFiltered { .. } | Self::Read { .. }
        )
    }
    pub(super) fn publication(&self, id: u32, reply_topic: &str) -> Publish {
        let (topic, payload) = match self {
            Self::ListAll => (
                "master/proxy/getDevList",
                json!({"info":["name","model","urn","online","specV2Access","pushAvailable"]}),
            ),
            Self::ListFiltered { dids } => (
                "master/proxy/getDevList",
                json!({"filter":{"did":dids},"info":["name","model","urn","online","specV2Access","pushAvailable"]}),
            ),
            Self::Read { did, binding, .. } => (
                "master/proxy/get",
                json!({"did":did,"siid":binding.siid,"piid":binding.piid}),
            ),
            Self::Write {
                did,
                binding,
                value,
            } => write_payload(did, &[(*binding, json!(value))]),
            Self::WriteProperties { did, properties } => write_payload(did, properties),
            Self::Subscribe { .. } | Self::Unsubscribe { .. } => {
                unreachable!("subscriptions do not publish MIPS requests")
            }
        };
        Publish::new(
            topic,
            QoS::ExactlyOnce,
            wire::encode_mips(id, Some(reply_topic), &payload),
        )
    }

    pub(super) fn operation(&self) -> &'static str {
        match self {
            Self::ListAll | Self::ListFiltered { .. } => "getDevList",
            Self::Subscribe {
                binding: NotificationBinding::Event(_),
                ..
            } => "subscribe event",
            Self::Subscribe { .. } => "subscribe",
            Self::Unsubscribe { .. } => "unsubscribe",
            Self::Read { .. } => "get",
            Self::Write { .. } | Self::WriteProperties { .. } => "set",
        }
    }
    pub(super) fn did(&self) -> &str {
        match self {
            Self::ListAll | Self::ListFiltered { .. } => "*",
            Self::Subscribe { did, .. }
            | Self::Unsubscribe { did, .. }
            | Self::Read { did, .. }
            | Self::Write { did, .. }
            | Self::WriteProperties { did, .. } => did,
        }
    }

    pub(super) fn cancelled_for(&self, did: &str, scope: CancellationScope) -> bool {
        !matches!(
            self,
            Self::ListAll | Self::ListFiltered { .. } | Self::Unsubscribe { .. }
        ) && self.did() == did
            && (scope == CancellationScope::Device
                || matches!(self, Self::Subscribe { binding, .. } if scope.includes(*binding)))
    }
}
pub(super) struct Shared {
    pub(super) queue: RefCell<VecDeque<Request>>,
    pub(super) event: Event,
    pub(super) closed: Cell<bool>,
    pub(super) outstanding: Cell<usize>,
    pub(super) cancellations: RefCell<VecDeque<Cancellation>>,
    pub(super) notification_invalidations: RefCell<BTreeSet<(String, NotificationBinding)>>,
    pub(super) cancel_unsent_business: Cell<bool>,
}

pub(super) struct Cancellation {
    pub(super) did: String,
    pub(super) scope: CancellationScope,
    pub(super) queued: VecDeque<Request>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum CancellationScope {
    Device,
    Properties,
    Events,
}
impl CancellationScope {
    pub(super) fn includes(self, binding: NotificationBinding) -> bool {
        matches!(
            (self, binding),
            (Self::Device, _)
                | (Self::Properties, NotificationBinding::Property(_))
                | (Self::Events, NotificationBinding::Event(_))
        )
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum NotificationBinding {
    Property(PropertyBinding),
    Event(EventBinding),
}
impl NotificationBinding {
    pub(super) fn topic(self, prefix: &str, did: &str) -> String {
        let (kind, siid, iid) = match self {
            Self::Property(binding) => ("property", binding.siid, binding.piid),
            Self::Event(binding) => ("event", binding.siid, binding.eiid),
        };
        format!("{prefix}/appMsg/notify/iot/{did}/{kind}/{siid}.{iid}")
    }
    fn valid(self) -> bool {
        match self {
            Self::Property(binding) => binding.siid > 0 && binding.piid > 0,
            Self::Event(binding) => binding.siid > 0 && binding.eiid > 0,
        }
    }
}

/// Requests are enqueued when these methods are called. Business callbacks and
/// waiters receive the same outcome, including send progress and timeout scope.
/// Dropping the future does not cancel a sent write or suppress its callback.
#[derive(Clone)]
pub struct SessionHandle {
    pub(super) shared: Rc<Shared>,
    pub(super) timing: Timing,
}
impl SessionHandle {
    /// Retire already received event deliveries, preserving their subscriptions.
    pub(crate) fn invalidate_received_event(&self, did: &str, binding: EventBinding) {
        self.invalidate_received(did, NotificationBinding::Event(binding));
    }

    /// Retire deliveries already received for one real property while keeping
    /// subscriptions, other functions and business requests intact.
    pub(crate) fn invalidate_received_property(&self, did: &str, binding: PropertyBinding) {
        self.invalidate_received(did, NotificationBinding::Property(binding));
    }
    fn invalidate_received(&self, did: &str, binding: NotificationBinding) {
        self.shared
            .notification_invalidations
            .borrow_mut()
            .insert((did.into(), binding));
        self.shared.event.notify(usize::MAX);
    }

    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.shared, &other.shared)
    }

    /// Retire only zero-byte lists and reads; writes and MQTT exchanges remain owned.
    pub fn cancel_unsent_business(&self) {
        self.shared.cancel_unsent_business.set(true);
        self.shared.event.notify(usize::MAX);
    }
    pub fn close(&self) {
        self.shared.closed.set(true);
        self.shared.event.notify(usize::MAX);
    }
    pub fn get_devices<F: FnOnce(RequestOutcome<Vec<DeviceEntry>>) + 'static>(
        &self,
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<Vec<DeviceEntry>>> + use<F> {
        self.submit(
            RequestKind::ListAll,
            self.timing.request,
            None,
            |response| match response {
                Response::Devices(devices) => devices,
                _ => unreachable!("device list request returns devices"),
            },
            on_result,
        )
    }
    /// Return only entries for the requested devices; missing entries stay absent.
    pub fn get_devices_filtered<F: FnOnce(RequestOutcome<Vec<DeviceEntry>>) + 'static>(
        &self,
        dids: &[String],
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<Vec<DeviceEntry>>> + use<F> {
        self.submit(
            RequestKind::ListFiltered {
                dids: dids.to_vec(),
            },
            self.timing.request,
            None,
            |response| match response {
                Response::Devices(devices) => devices,
                _ => unreachable!("device list request returns devices"),
            },
            on_result,
        )
    }

    /// Cancel this device's existing work before the driver processes another frame.
    pub fn cancel_device(&self, did: &str) -> Result<(), CentralError> {
        self.cancel(did, CancellationScope::Device)
    }

    fn cancel(&self, did: &str, scope: CancellationScope) -> Result<(), CentralError> {
        if !valid_identifier(did) {
            return Err(CentralError::InvalidInput);
        }
        if self.shared.closed.get() {
            return Err(CentralError::Cancelled);
        }
        let mut queue = self.shared.queue.borrow_mut();
        let (queued, retained) = queue
            .drain(..)
            .partition(|request| request.kind.cancelled_for(did, scope));
        *queue = retained;
        self.shared
            .cancellations
            .borrow_mut()
            .push_back(Cancellation {
                did: did.into(),
                scope,
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
        let result = self.cancel_device(did).map(|()| {
            self.submit_subscription(RequestKind::Unsubscribe {
                did: did.into(),
                scope: CancellationScope::Device,
            })
        });
        async move { result?.await }
    }

    /// Retire property subscriptions without cancelling in-flight reads or writes.
    /// Late SUBACKs cannot reinstate the retired subscriptions.
    pub fn unsubscribe_properties(
        &self,
        did: &str,
    ) -> impl Future<Output = Result<(), CentralError>> + use<> {
        let result = self.cancel(did, CancellationScope::Properties).map(|()| {
            self.submit_subscription(RequestKind::Unsubscribe {
                did: did.into(),
                scope: CancellationScope::Properties,
            })
        });
        async move { result?.await }
    }

    /// Retire event subscriptions without cancelling properties or business requests.
    pub fn unsubscribe_events(
        &self,
        did: &str,
    ) -> impl Future<Output = Result<(), CentralError>> + use<> {
        let result = self.cancel(did, CancellationScope::Events).map(|()| {
            self.submit_subscription(RequestKind::Unsubscribe {
                did: did.into(),
                scope: CancellationScope::Events,
            })
        });
        async move { result?.await }
    }

    pub fn subscribe_property<F: FnOnce(Result<(), CentralError>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        on_result: F,
    ) -> impl Future<Output = Result<(), CentralError>> + use<F> {
        self.subscribe(did, NotificationBinding::Property(binding), on_result)
    }
    pub fn subscribe_event<F: FnOnce(Result<(), CentralError>) + 'static>(
        &self,
        did: &str,
        binding: EventBinding,
        on_result: F,
    ) -> impl Future<Output = Result<(), CentralError>> + use<F> {
        self.subscribe(did, NotificationBinding::Event(binding), on_result)
    }
    fn subscribe<F: FnOnce(Result<(), CentralError>) + 'static>(
        &self,
        did: &str,
        binding: NotificationBinding,
        on_result: F,
    ) -> impl Future<Output = Result<(), CentralError>> + use<F> {
        let submitted = self.submit(
            RequestKind::Subscribe {
                did: did.into(),
                binding,
            },
            self.timing.request,
            None,
            |_| (),
            move |outcome| on_result(outcome.result),
        );
        async move { submitted.await.result }
    }
    pub fn set_property_guarded<F: FnOnce(RequestOutcome<()>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        value: bool,
        hooks: SendHooks,
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<()>> + use<F> {
        self.submit(
            RequestKind::Write {
                did: did.into(),
                binding,
                value,
            },
            self.timing.request,
            Some(hooks),
            |_| (),
            on_result,
        )
    }
    pub fn read_property_observation_with_timeout_guarded<
        F: FnOnce(RequestOutcome<PropertyReading>) + 'static,
    >(
        &self,
        did: &str,
        binding: PropertyBinding,
        timeout: Duration,
        hooks: SendHooks,
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<PropertyReading>> + use<F> {
        self.submit(
            RequestKind::Read {
                did: did.into(),
                binding,
            },
            timeout.min(self.timing.request),
            Some(hooks),
            |response| match response {
                Response::PropertyValue(reading) => reading,
                _ => unreachable!("property read returns a supported value"),
            },
            on_result,
        )
    }
    pub fn set_properties_guarded<
        F: FnOnce(RequestOutcome<PropertyResults<(), CentralError>>) + 'static,
    >(
        &self,
        did: &str,
        properties: &[(PropertyBinding, Value)],
        hooks: SendHooks,
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<PropertyResults<(), CentralError>>> + use<F> {
        self.submit(
            RequestKind::WriteProperties {
                did: did.into(),
                properties: properties.to_vec(),
            },
            self.timing.request,
            Some(hooks),
            |response| match response {
                Response::Properties(results) => results,
                _ => unreachable!("batch write returns property results"),
            },
            on_result,
        )
    }
    fn submit_subscription(
        &self,
        kind: RequestKind,
    ) -> impl Future<Output = Result<(), CentralError>> + use<> {
        let submitted = self.submit(kind, self.timing.request, None, |_| (), |_| {});
        async move { submitted.await.result }
    }
    fn submit<T: Clone + 'static, F: FnOnce(RequestOutcome<T>) + 'static>(
        &self,
        kind: RequestKind,
        timeout: Duration,
        hooks: Option<SendHooks>,
        decode: fn(Response) -> T,
        on_result: F,
    ) -> impl Future<Output = RequestOutcome<T>> + use<T, F> {
        let completion = Rc::new(Completion {
            result: RefCell::new(None),
            event: Event::new(),
        });
        let request = Request {
            kind,
            deadline: Instant::now() + timeout,
            complete: Box::new({
                let completion = completion.clone();
                move |result, stage, timeout| {
                    let outcome = RequestOutcome {
                        result: result.map(decode),
                        stage,
                        timeout,
                    };
                    on_result(outcome.clone());
                    *completion.result.borrow_mut() = Some(outcome);
                    completion.event.notify(usize::MAX);
                }
            }),
            id: None,
            stage: RequestStage::NotSent,
            hooks,
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
            (request.complete)(Err(error), RequestStage::NotSent, None);
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
        RequestKind::ListFiltered { dids } => {
            return !dids.is_empty() && dids.iter().all(|did| valid_identifier(did));
        }
        RequestKind::Unsubscribe { did, .. } => (did, None),
        RequestKind::Subscribe { did, binding } => {
            return valid_identifier(did) && binding.valid();
        }
        RequestKind::WriteProperties { did, properties } => {
            return valid_identifier(did)
                && valid_bindings(
                    &properties
                        .iter()
                        .map(|(binding, _)| *binding)
                        .collect::<Vec<_>>(),
                )
                && properties.iter().all(|(_, value)| valid_value(value));
        }
        RequestKind::Read { did, binding } | RequestKind::Write { did, binding, .. } => {
            (did, Some(binding))
        }
    };
    valid_identifier(did) && binding.is_none_or(|binding| binding.siid > 0 && binding.piid > 0)
}

fn write_payload(did: &str, properties: &[(PropertyBinding, Value)]) -> (&'static str, Value) {
    let params: Vec<_> = properties.iter().map(|(binding, value)| {
        json!({"did":did,"siid":binding.siid,"piid":binding.piid,"value":value})
    }).collect();
    (
        "master/proxy/rpcReq",
        json!({"did":did,"rpc":{"id":next_mips_id(),"method":"set_properties","params":params}}),
    )
}

pub(super) fn next_mips_id() -> u32 {
    // MIPS uses a wrapping 32-bit sequence. Keep it across connections so a
    // reconnect cannot immediately reuse an outstanding request's identifier.
    static IDS: OnceLock<AtomicU32> = OnceLock::new();
    IDS.get_or_init(|| AtomicU32::new(rand::random()))
        .fetch_add(1, Ordering::Relaxed)
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
            Ok(Response::PropertyValue(PropertyReading {
                value: value
                    .get("value")
                    .filter(|value| valid_value(value))
                    .cloned()
                    .ok_or(CentralError::Protocol)?,
                update_time: update_time(value),
            }))
        }
        RequestKind::WriteProperties { did, properties } => {
            let result = value
                .get("result")
                .and_then(Value::as_array)
                .ok_or(CentralError::Protocol)?;
            let bindings: Vec<_> = properties.iter().map(|(binding, _)| *binding).collect();
            Ok(Response::Properties(correlate(
                did,
                &bindings,
                result,
                CentralError::Protocol,
                |item| match item
                    .get("code")
                    .and_then(Value::as_i64)
                    .ok_or(CentralError::Protocol)?
                {
                    0 | 1 => Ok(()),
                    code => Err(CentralError::Rejected(code)),
                },
            )))
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
        RequestKind::ListAll | RequestKind::ListFiltered { .. } => {
            let list = value
                .get("devList")
                .and_then(Value::as_object)
                .ok_or(CentralError::Protocol)?;
            let mut entries = Vec::new();
            for (did, info) in list {
                if let RequestKind::ListFiltered { dids } = kind
                    && !dids.contains(did)
                {
                    continue;
                }
                if !valid_identifier(did) {
                    return Err(CentralError::Protocol);
                }
                entries.push(DeviceEntry {
                    did: did.clone(),
                    access: device_access(did, info),
                });
            }
            Ok(Response::Devices(entries))
        }
        RequestKind::Subscribe { .. } | RequestKind::Unsubscribe { .. } => {
            Err(CentralError::Protocol)
        }
    }
}

fn device_access(did: &str, info: &Value) -> Result<DeviceAccess, CentralError> {
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
