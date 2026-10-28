use super::attachment::Prepare;
use super::*;

impl DeviceWork {
    pub(super) fn channel_bindings(&self, id: &str) -> Vec<PropertyBinding> {
        if id == BATTERY_CHANNEL {
            return self.battery_binding().into_iter().collect();
        }
        let function = self
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap();
        if function.kind().is_read_only() || function.curtain().is_some() {
            return function.observation_bindings();
        }
        let mut bindings = self
            .native
            .device()
            .functions()
            .iter()
            .filter(|function| function.kind().is_control())
            .flat_map(ParsedFunction::bindings)
            .collect::<Vec<_>>();
        bindings.extend(self.native.device().refresh_trigger());
        bindings
    }
    pub(super) fn channel_subscriptions(&self, id: &str) -> Vec<SubscriptionBinding> {
        let mut subscriptions = self
            .channel_bindings(id)
            .into_iter()
            .map(SubscriptionBinding::Property)
            .collect::<Vec<_>>();
        if let Some(event) = self
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .and_then(ParsedFunction::motion)
            .map(|motion| motion.event())
        {
            subscriptions.push(SubscriptionBinding::Event(event));
        }
        subscriptions
    }
    pub(super) fn required_subscriptions(&self) -> Vec<SubscriptionBinding> {
        let mut values = self
            .required_bindings()
            .into_iter()
            .map(SubscriptionBinding::Property)
            .collect::<Vec<_>>();
        values.extend(
            self.native
                .device()
                .functions()
                .iter()
                .filter_map(ParsedFunction::motion)
                .map(|motion| SubscriptionBinding::Event(motion.event())),
        );
        values
    }
    pub(super) fn subscriptions(&self) -> Vec<SubscriptionBinding> {
        let mut values = self.required_subscriptions();
        values.extend(self.battery_binding().map(SubscriptionBinding::Property));
        values
    }
    pub(in crate::xiaomi::adapter) fn subscription_status(
        &self,
        id: &str,
    ) -> Vec<SubscriptionStatus> {
        let bindings = self.channel_subscriptions(id);
        let local = self.local.borrow();
        let cloud = self.cloud.borrow();
        [
            (PathKind::Local, &local.prepare),
            (PathKind::Cloud, &cloud.prepare),
        ]
        .into_iter()
        .flat_map(|(source, prepare)| {
            bindings.iter().map(move |binding| SubscriptionStatus {
                binding: *binding,
                source,
                ready: prepare.subscribed.contains(binding),
                problem: prepare.problems.get(binding).cloned(),
            })
        })
        .collect()
    }
    pub(in crate::xiaomi::adapter) fn request_local_subscriptions(&self) {
        let mut local = self.local.borrow_mut();
        if local.connection.is_none() || local.prepare.running || local.prepare.exhausted {
            return;
        }
        if !local.requested {
            local.requested = true;
            drop(local);
            self.changed.notify(usize::MAX);
        }
    }
    pub(super) async fn local_preparation_request(&self) -> (SessionHandle, u64) {
        loop {
            let listener = self.changed.listen();
            let request = {
                let mut local = self.local.borrow_mut();
                let connection = local.connection.clone();
                if let Some(connection) = connection
                    && local.requested
                    && let Some(generation) = local.prepare.begin()
                {
                    local.requested = false;
                    Some((connection, generation))
                } else {
                    None
                }
            };
            if let Some(request) = request {
                return request;
            }
            listener.await;
        }
    }
    pub(super) fn local_current(&self, handle: &SessionHandle, generation: u64) -> bool {
        !self.life.is_closed()
            && self
                .local
                .borrow()
                .connection
                .as_ref()
                .is_some_and(|current| {
                    current.same_connection(handle)
                        && self.local.borrow().prepare.generation == generation
                })
    }
    pub(super) fn local_pending(
        &self,
        handle: &SessionHandle,
        generation: u64,
    ) -> Option<Vec<SubscriptionBinding>> {
        let local = self.local.borrow();
        let current = local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && local.prepare.generation == generation
            && !self.life.is_closed();
        current.then(|| local.prepare.missing(&self.subscriptions()))
    }
    pub(in crate::xiaomi::adapter) fn local_subscribed(
        &self,
        handle: &SessionHandle,
        generation: u64,
        binding: SubscriptionBinding,
    ) -> bool {
        let mut local = self.local.borrow_mut();
        if local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && local.prepare.generation == generation
            && !self.life.is_closed()
        {
            if self.battery_binding().map(SubscriptionBinding::Property) == Some(binding) {
                self.set_battery_subscription_problem(None);
            }
            local.prepare.subscribed(binding);
            true
        } else {
            false
        }
    }
    pub(in crate::xiaomi::adapter) fn subscription_failed(
        &self,
        source: &NotificationSource,
        generation: u64,
        binding: SubscriptionBinding,
        problem: String,
    ) {
        let record = |prepare: &mut Prepare| {
            if prepare.generation == generation {
                prepare.problems.insert(binding, problem);
            }
        };
        match source {
            NotificationSource::Local(handle) if self.local_attached(handle) => {
                record(&mut self.local.borrow_mut().prepare)
            }
            NotificationSource::Cloud(handle) if self.cloud_attached(handle) => {
                record(&mut self.cloud.borrow_mut().prepare)
            }
            _ => {}
        }
    }
    pub(in crate::xiaomi::adapter) fn function_subscription_problem(
        &self,
        id: &str,
    ) -> Option<String> {
        let local = self.local.borrow();
        let cloud = self.cloud.borrow();
        let mut problems = Vec::new();
        for binding in self.channel_subscriptions(id) {
            problems.extend(
                [
                    local.prepare.problems.get(&binding),
                    cloud.prepare.problems.get(&binding),
                ]
                .into_iter()
                .flatten()
                .cloned(),
            );
        }
        (!problems.is_empty()).then(|| problems.join("; "))
    }
    pub(in crate::xiaomi::adapter) fn local_finished(
        &self,
        handle: &SessionHandle,
        generation: u64,
    ) {
        let battery = self.battery_binding();
        let mut local = self.local.borrow_mut();
        if local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && local.prepare.generation == generation
            && let Some(problem) = local.prepare.finish(battery)
        {
            self.set_battery_subscription_problem(Some(problem));
        }
    }
    pub(super) fn local_abandoned(&self, handle: &SessionHandle, generation: u64) {
        let mut local = self.local.borrow_mut();
        if local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && local.prepare.generation == generation
        {
            local.prepare.running = false;
        }
    }
    pub(in crate::xiaomi::adapter) fn begin_cloud_preparation(
        &self,
        handle: &NotificationHandle,
    ) -> Option<u64> {
        let mut cloud = self.cloud.borrow_mut();
        if !cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            || self.life.is_closed()
        {
            return None;
        }
        cloud.prepare.begin()
    }
    pub(in crate::xiaomi::adapter) fn cloud_current(
        &self,
        handle: &NotificationHandle,
        generation: u64,
    ) -> bool {
        !self.life.is_closed()
            && self
                .cloud
                .borrow()
                .connection
                .as_ref()
                .is_some_and(|current| {
                    current.same_connection(handle)
                        && self.cloud.borrow().prepare.generation == generation
                })
    }
    pub(in crate::xiaomi::adapter) fn cloud_pending(
        &self,
        handle: &NotificationHandle,
        generation: u64,
    ) -> Option<Vec<SubscriptionBinding>> {
        let cloud = self.cloud.borrow();
        let current = cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && cloud.prepare.generation == generation
            && !self.life.is_closed();
        current.then(|| cloud.prepare.missing(&self.subscriptions()))
    }
    pub(in crate::xiaomi::adapter) fn cloud_subscribed(
        &self,
        handle: &NotificationHandle,
        generation: u64,
        binding: SubscriptionBinding,
    ) -> bool {
        let mut cloud = self.cloud.borrow_mut();
        if cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && cloud.prepare.generation == generation
            && !self.life.is_closed()
        {
            if self.battery_binding().map(SubscriptionBinding::Property) == Some(binding) {
                self.set_battery_subscription_problem(None);
            }
            cloud.prepare.subscribed(binding);
            true
        } else {
            false
        }
    }
    pub(in crate::xiaomi::adapter) fn cloud_finished(
        &self,
        handle: &NotificationHandle,
        generation: u64,
    ) {
        let battery = self.battery_binding();
        let mut cloud = self.cloud.borrow_mut();
        if cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && cloud.prepare.generation == generation
            && let Some(problem) = cloud.prepare.finish(battery)
        {
            self.set_battery_subscription_problem(Some(problem));
        }
    }
    pub(in crate::xiaomi::adapter) fn cloud_abandoned(
        &self,
        handle: &NotificationHandle,
        generation: u64,
    ) {
        let mut cloud = self.cloud.borrow_mut();
        if cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && cloud.prepare.generation == generation
        {
            cloud.prepare.running = false;
        }
    }
    pub(in crate::xiaomi::adapter) fn source_ready(
        &self,
        id: &str,
        source: &NotificationSource,
    ) -> bool {
        let bindings = self.channel_subscriptions(id);
        match source {
            NotificationSource::Local(handle) => {
                let local = self.local.borrow();
                !self.life.is_closed()
                    && local
                        .connection
                        .as_ref()
                        .is_some_and(|current| current.same_connection(handle))
                    && local.prepare.ready(&bindings)
            }
            NotificationSource::Cloud(handle) => {
                let cloud = self.cloud.borrow();
                !self.life.is_closed()
                    && cloud
                        .connection
                        .as_ref()
                        .is_some_and(|current| current.same_connection(handle))
                    && cloud.prepare.ready(&bindings)
            }
        }
    }
    pub(in crate::xiaomi::adapter) fn diagnostic(&self) -> Option<String> {
        let required = self.required_subscriptions();
        let local = self.local.borrow();
        let values = local
            .prepare
            .problem(&required)
            .into_iter()
            .chain(self.cloud.borrow().prepare.problem(&required))
            .chain(self.refresh.borrow().problems_except(BATTERY_CHANNEL))
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }
    pub(in crate::xiaomi::adapter) fn battery_problem(&self) -> Option<String> {
        let mut values = Vec::new();
        if let Some(issue) = self.native.device().battery_issue() {
            values.push(issue.to_owned());
        }
        if let Some(problem) = self.battery_subscription_problem.borrow().clone() {
            values.push(problem);
        }
        if let Some(problem) = self.battery_read_problem.borrow().clone() {
            values.push(problem);
        }
        (!values.is_empty()).then(|| values.join("; "))
    }
    pub(in crate::xiaomi::adapter) fn set_battery_subscription_problem(
        &self,
        problem: Option<String>,
    ) {
        *self.battery_subscription_problem.borrow_mut() = problem;
    }
    pub(in crate::xiaomi::adapter) fn set_battery_read_problem(&self, problem: Option<String>) {
        *self.battery_read_problem.borrow_mut() = problem;
    }
    pub(in crate::xiaomi::adapter) fn subscription_problem(&self) -> Option<String> {
        let required = self.required_subscriptions();
        self.local
            .borrow()
            .prepare
            .problem(&required)
            .or_else(|| self.cloud.borrow().prepare.problem(&required))
    }
    pub(in crate::xiaomi::adapter) fn report(&self, context: &Context, did: &str) {
        if self.life.is_closed() {
            return;
        }
        let blocked = context.live.borrow().devices[did].blocked.clone();
        let problem = blocked.or_else(|| self.diagnostic());
        context.problem(did, problem);
    }
    pub(in crate::xiaomi::adapter) fn problem(
        &self,
        operation: &str,
        binding: PropertyBinding,
        error: &impl std::fmt::Display,
    ) -> String {
        let message = format!(
            "{} {operation} {}.{}: {error}",
            self.key.id, binding.siid, binding.piid
        );
        log::warn!("{message}");
        message
    }
}

pub(super) async fn subscriptions(
    work: &Rc<DeviceWork>,
    handle: &SessionHandle,
    context: &Rc<Context>,
) -> Result<(), AdapterError> {
    let did = work.key.id.clone();
    loop {
        let (connection, preparation) = work.local_preparation_request().await;
        if !connection.same_connection(handle) {
            work.local_abandoned(handle, preparation);
            continue;
        }
        if !context.current_local(handle) || !work.local_current(handle, preparation) {
            work.local_abandoned(handle, preparation);
            continue;
        }
        context
            .clock
            .wait_until(context.clock.now() + Duration::from_millis(10))
            .await;
        let current = || context.current_local(handle) && work.local_current(handle, preparation);
        let Some(mut pending) = work.local_pending(handle, preparation) else {
            continue;
        };
        'attempts: for attempt in 0..4 {
            let mut failed = Vec::new();
            for binding in pending {
                if !current() {
                    break 'attempts;
                }
                let receipt_work = work.clone();
                let receipt_context = context.clone();
                let receipt_handle = handle.clone();
                let on_result = move |result: Result<(), crate::xiaomi::central::CentralError>| {
                    if result.is_ok()
                        && receipt_context.current_local(&receipt_handle)
                        && receipt_work.local_subscribed(&receipt_handle, preparation, binding)
                    {
                        receipt_context.changed.notify(usize::MAX);
                    }
                };
                let result = match binding {
                    SubscriptionBinding::Property(binding) => {
                        handle.subscribe_property(&did, binding, on_result).await
                    }
                    SubscriptionBinding::Event(binding) => {
                        handle.subscribe_event(&did, binding, on_result).await
                    }
                };
                if !current() {
                    break 'attempts;
                }
                match result {
                    Ok(()) => {}
                    Err(error) => {
                        let message = format!("{did} subscribe {binding}: {error}");
                        work.subscription_failed(
                            &NotificationSource::Local(handle.clone()),
                            preparation,
                            binding,
                            message,
                        );
                        failed.push(binding);
                    }
                }
            }
            if failed.is_empty() || attempt == 3 {
                work.local_finished(handle, preparation);
                work.report(context, &did);
                break;
            }
            pending = failed;
            context
                .clock
                .wait_until(context.clock.now() + Duration::from_millis(100))
                .await;
        }
        work.local_abandoned(handle, preparation);
        context.changed.notify(usize::MAX);
    }
}
