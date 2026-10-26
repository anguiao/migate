use super::{LIMIT, NotificationError, Timing};
use crate::xiaomi::capability::{EventBinding, PropertyBinding};
use event_listener::Event;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    future::Future,
    rc::Rc,
    time::Instant,
};

type CompletionObserver = Box<dyn FnOnce(Result<(), NotificationError>)>;
pub(super) struct Completion {
    result: RefCell<Option<Result<(), NotificationError>>>,
    event: Event,
    observers: RefCell<Vec<CompletionObserver>>,
}

impl Completion {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            result: RefCell::new(None),
            event: Event::new(),
            observers: RefCell::new(Vec::new()),
        })
    }
    pub(super) fn finish(&self, result: Result<(), NotificationError>) -> bool {
        if self.result.borrow().is_some() {
            return false;
        }
        *self.result.borrow_mut() = Some(result.clone());
        for observer in std::mem::take(&mut *self.observers.borrow_mut()) {
            observer(result.clone());
        }
        self.event.notify(usize::MAX);
        true
    }
    async fn wait(self: Rc<Self>) -> Result<(), NotificationError> {
        loop {
            let listener = self.event.listen();
            if let Some(result) = self.result.borrow().clone() {
                return result;
            }
            listener.await;
        }
    }
}

#[derive(Clone)]
pub(super) enum Target {
    Event {
        did: String,
        binding: EventBinding,
    },
    Property {
        did: String,
        binding: PropertyBinding,
    },
    Online {
        did: String,
    },
}

impl Target {
    pub(super) fn did(&self) -> &str {
        match self {
            Self::Property { did, .. } | Self::Event { did, .. } | Self::Online { did } => did,
        }
    }
    fn retired_by(&self, did: &str, scope: Retire) -> bool {
        if self.did() != did {
            return false;
        }
        match scope {
            Retire::Device => true,
            Retire::Properties => matches!(self, Self::Property { .. }),
            Retire::Events => matches!(self, Self::Event { .. }),
            Retire::Online => matches!(self, Self::Online { .. }),
        }
    }
}

/// Property, event and online subscriptions have independent ownership.
#[derive(Clone, Copy)]
pub(super) enum Retire {
    Device,
    Properties,
    Events,
    Online,
}

pub(super) struct Registration {
    pub(super) target: Target,
    pub(super) generation: u64,
    pub(super) delivery_generation: u64,
    pub(super) ready: bool,
    completion: Rc<Completion>,
}

pub(super) enum RequestKind {
    Subscribe { topic: String, generation: u64 },
    Unsubscribe { filters: Vec<String> },
}

pub(super) struct Request {
    pub(super) kind: RequestKind,
    pub(super) deadline: Instant,
    pub(super) completion: Rc<Completion>,
}

pub(super) struct Shared {
    pub(super) queue: RefCell<VecDeque<Request>>,
    pub(super) registrations: RefCell<HashMap<String, Registration>>,
    known_topics: RefCell<HashMap<String, Target>>,
    generation: Cell<u64>,
    pub(super) outstanding: Cell<usize>,
    pub(super) event: Event,
    pub(super) closed: Cell<bool>,
}

impl Shared {
    pub(super) fn new() -> Rc<Self> {
        Rc::new(Self {
            queue: Default::default(),
            registrations: Default::default(),
            known_topics: Default::default(),
            generation: Cell::new(0),
            outstanding: Cell::new(0),
            event: Event::new(),
            closed: Cell::new(false),
        })
    }
    pub(super) fn current(&self, topic: &str, generation: u64) -> bool {
        self.registrations
            .borrow()
            .get(topic)
            .is_some_and(|registration| registration.generation == generation)
    }
    pub(super) fn current_delivery(
        &self,
        topic: &str,
        generation: u64,
        delivery_generation: u64,
    ) -> bool {
        self.registrations
            .borrow()
            .get(topic)
            .is_some_and(|registration| {
                registration.generation == generation
                    && registration.delivery_generation == delivery_generation
            })
    }
    pub(super) fn finish(&self, request: &Request, result: Result<(), NotificationError>) {
        if request.completion.finish(result.clone()) {
            self.outstanding.set(self.outstanding.get() - 1);
        }
        if result.is_err()
            && let RequestKind::Subscribe { topic, generation } = &request.kind
            && self.current(topic, *generation)
        {
            self.registrations.borrow_mut().remove(topic);
        }
    }
}

#[derive(Clone)]
pub struct NotificationHandle {
    pub(super) shared: Rc<Shared>,
    pub(super) timing: Timing,
}

impl NotificationHandle {
    /// Retire received event deliveries without changing subscription readiness.
    pub(crate) fn invalidate_received_event(&self, did: &str, binding: EventBinding) {
        for registration in self.shared.registrations.borrow_mut().values_mut() {
            if matches!(&registration.target, Target::Event { did: target, binding: event } if target == did && *event == binding)
            {
                registration.delivery_generation = registration.delivery_generation.wrapping_add(1);
            }
        }
    }

    /// Retire received deliveries for one function's property while preserving
    /// other property generations, subscription readiness and online reports.
    pub(crate) fn invalidate_received_property(&self, did: &str, binding: PropertyBinding) {
        for registration in self.shared.registrations.borrow_mut().values_mut() {
            if matches!(&registration.target, Target::Property { did: target, binding: property } if target == did && *property == binding)
            {
                registration.delivery_generation = registration.delivery_generation.wrapping_add(1);
            }
        }
    }

    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.shared, &other.shared)
    }

    pub fn subscribe_property(
        &self,
        did: &str,
        binding: PropertyBinding,
    ) -> impl Future<Output = Result<(), NotificationError>> + use<> {
        self.subscribe_property_with_result(did, binding, |_| {})
    }

    pub fn subscribe_online(
        &self,
        did: &str,
    ) -> impl Future<Output = Result<(), NotificationError>> + use<> {
        let completion = if !valid_identifier(did) {
            Err(NotificationError::InvalidInput)
        } else if did.starts_with("blt.") || did.starts_with("proxy.") {
            self.immediate(Ok(()))
        } else {
            self.subscribe(
                format!("device/{did}/state/#"),
                Target::Online { did: did.into() },
            )
        };
        async move { completion?.wait().await }
    }

    pub fn subscribe_event(
        &self,
        did: &str,
        binding: EventBinding,
    ) -> impl Future<Output = Result<(), NotificationError>> + use<> {
        self.subscribe_event_with_result(did, binding, |_| {})
    }

    pub(crate) fn subscribe_property_with_result<
        F: FnOnce(Result<(), NotificationError>) + 'static,
    >(
        &self,
        did: &str,
        binding: PropertyBinding,
        on_result: F,
    ) -> impl Future<Output = Result<(), NotificationError>> + use<F> {
        let completion = if binding.siid == 0 || binding.piid == 0 {
            Err(NotificationError::InvalidInput)
        } else {
            self.subscribe(
                format!(
                    "device/{did}/up/properties_changed/{}/{}",
                    binding.siid, binding.piid
                ),
                Target::Property {
                    did: did.into(),
                    binding,
                },
            )
        };
        Self::observed_subscription(completion, on_result)
    }
    pub(crate) fn subscribe_event_with_result<
        F: FnOnce(Result<(), NotificationError>) + 'static,
    >(
        &self,
        did: &str,
        binding: EventBinding,
        on_result: F,
    ) -> impl Future<Output = Result<(), NotificationError>> + use<F> {
        let completion = if binding.siid == 0 || binding.eiid == 0 {
            Err(NotificationError::InvalidInput)
        } else {
            self.subscribe(
                format!(
                    "device/{did}/up/event_occured/{}/{}",
                    binding.siid, binding.eiid
                ),
                Target::Event {
                    did: did.into(),
                    binding,
                },
            )
        };
        Self::observed_subscription(completion, on_result)
    }
    fn observed_subscription(
        completion: Result<Rc<Completion>, NotificationError>,
        on_result: impl FnOnce(Result<(), NotificationError>) + 'static,
    ) -> impl Future<Output = Result<(), NotificationError>> {
        match &completion {
            Ok(completion) => {
                let result = completion.result.borrow().clone();
                if let Some(result) = result {
                    on_result(result);
                } else {
                    completion.observers.borrow_mut().push(Box::new(on_result));
                }
            }
            Err(error) => on_result(Err(error.clone())),
        }
        async move { completion?.wait().await }
    }
    fn immediate(
        &self,
        result: Result<(), NotificationError>,
    ) -> Result<Rc<Completion>, NotificationError> {
        if self.shared.closed.get() {
            return Err(NotificationError::Cancelled);
        }
        let completion = Completion::new();
        completion.finish(result);
        Ok(completion)
    }

    fn subscribe(
        &self,
        topic: String,
        target: Target,
    ) -> Result<Rc<Completion>, NotificationError> {
        if !valid_identifier(target.did()) {
            return Err(NotificationError::InvalidInput);
        }
        if self.shared.closed.get() {
            return Err(NotificationError::Cancelled);
        }
        if let Some(registration) = self.shared.registrations.borrow().get(&topic) {
            return Ok(registration.completion.clone());
        }
        if self.shared.outstanding.get() >= LIMIT {
            return Err(NotificationError::QueueFull);
        }
        let generation = self.shared.generation.get().wrapping_add(1);
        self.shared.generation.set(generation);
        let completion = Completion::new();
        self.shared.registrations.borrow_mut().insert(
            topic.clone(),
            Registration {
                target: target.clone(),
                generation,
                delivery_generation: 0,
                ready: false,
                completion: completion.clone(),
            },
        );
        self.shared
            .known_topics
            .borrow_mut()
            .insert(topic.clone(), target);
        self.enqueue(
            RequestKind::Subscribe { topic, generation },
            completion.clone(),
        );
        Ok(completion)
    }

    pub fn unsubscribe_properties(
        &self,
        did: &str,
    ) -> impl Future<Output = Result<(), NotificationError>> + use<> {
        let completion = self.unsubscribe(did, Retire::Properties);
        async move { completion?.wait().await }
    }

    pub fn unsubscribe_events(
        &self,
        did: &str,
    ) -> impl Future<Output = Result<(), NotificationError>> + use<> {
        let completion = self.unsubscribe(did, Retire::Events);
        async move { completion?.wait().await }
    }

    /// Retire only online notifications. Property subscriptions have their own
    /// lifecycle and survive an online reconcile.
    pub fn unsubscribe_online(
        &self,
        did: &str,
    ) -> impl Future<Output = Result<(), NotificationError>> + use<> {
        let completion = self.unsubscribe(did, Retire::Online);
        async move { completion?.wait().await }
    }

    pub fn unsubscribe_device(
        &self,
        did: &str,
    ) -> impl Future<Output = Result<(), NotificationError>> + use<> {
        let completion = self.unsubscribe(did, Retire::Device);
        async move { completion?.wait().await }
    }

    fn unsubscribe(&self, did: &str, scope: Retire) -> Result<Rc<Completion>, NotificationError> {
        if !valid_identifier(did) {
            return Err(NotificationError::InvalidInput);
        }
        if self.shared.closed.get() {
            return Err(NotificationError::Cancelled);
        }
        let removed: Vec<_> = self
            .shared
            .registrations
            .borrow_mut()
            .extract_if(|_, registration| registration.target.retired_by(did, scope))
            .collect();
        for (_, registration) in removed {
            if registration
                .completion
                .finish(Err(NotificationError::Cancelled))
            {
                self.shared
                    .outstanding
                    .set(self.shared.outstanding.get() - 1);
            }
        }
        // Cancelled requests that have not reached the driver leave the queue
        // before unsubscription is added, keeping its capacity bounded.
        self.shared.queue.borrow_mut().retain(|request| {
            !matches!(&request.kind, RequestKind::Subscribe { topic, generation }
                if !self.shared.current(topic, *generation))
        });
        self.shared.event.notify(usize::MAX);
        let filters: Vec<_> = self
            .shared
            .known_topics
            .borrow()
            .iter()
            .filter_map(|(topic, target)| target.retired_by(did, scope).then_some(topic.clone()))
            .collect();
        if filters.is_empty() {
            return self.immediate(Ok(()));
        }
        if self.shared.outstanding.get() >= LIMIT {
            return Err(NotificationError::QueueFull);
        }
        self.shared
            .known_topics
            .borrow_mut()
            .retain(|_, target| !target.retired_by(did, scope));
        let completion = Completion::new();
        self.enqueue(RequestKind::Unsubscribe { filters }, completion.clone());
        Ok(completion)
    }

    fn enqueue(&self, kind: RequestKind, completion: Rc<Completion>) {
        self.shared.queue.borrow_mut().push_back(Request {
            kind,
            deadline: Instant::now() + self.timing.request,
            completion,
        });
        self.shared
            .outstanding
            .set(self.shared.outstanding.get() + 1);
        self.shared.event.notify(usize::MAX);
    }

    pub fn close(&self) {
        self.shared.closed.set(true);
        self.shared.event.notify(usize::MAX);
    }
}

pub(super) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= u16::MAX as usize
        && !value.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '/' | '+' | '#')
        })
}
