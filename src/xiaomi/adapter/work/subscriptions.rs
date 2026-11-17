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
        if function.relay_mode().is_some() {
            return function.notification_bindings();
        }
        if function.lighting().is_some() {
            let mut bindings = function.observation_bindings();
            bindings.extend(self.native.device().refresh_trigger());
            return bindings;
        }
        if function.kind().is_read_only()
            || function.curtain().is_some()
            || function.fan().is_some()
            || function.air_conditioner().is_some()
            || function.heater().is_some()
        {
            let mut bindings = function.observation_bindings();
            if let Some(heater) = function.heater() {
                bindings.extend(heater.diagnostic_bindings());
                bindings.extend(
                    heater
                        .power_refresh_trigger()
                        .map(|trigger| trigger.property),
                );
            }
            return bindings;
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
        let local = self.attachments.local.borrow();
        let cloud = self.attachments.cloud.borrow();
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
        let mut local = self.attachments.local.borrow_mut();
        if local.connection.is_none() || local.prepare.running || local.prepare.exhausted {
            return;
        }
        if !local.requested {
            local.requested = true;
            drop(local);
            self.attachments.changed.notify(usize::MAX);
        }
    }
    pub(super) async fn local_preparation_request(&self) -> (SessionHandle, u64) {
        loop {
            let listener = self.attachments.changed.listen();
            let connection = {
                let mut local = self.attachments.local.borrow_mut();
                local.requested.then(|| {
                    local.requested = false;
                    local.connection.as_ref().unwrap().clone()
                })
            };
            if let Some(connection) = connection
                && let Some(generation) =
                    self.begin_preparation(&NotificationSource::Local(connection.clone()))
            {
                return (connection, generation);
            }
            listener.await;
        }
    }
    pub(in crate::xiaomi::adapter) fn begin_preparation(
        &self,
        source: &NotificationSource,
    ) -> Option<u64> {
        if self.life.is_closed() {
            return None;
        }
        self.attachments.prepare_mut(source)?.begin()
    }
    pub(in crate::xiaomi::adapter) fn preparation_current(
        &self,
        source: &NotificationSource,
        generation: u64,
    ) -> bool {
        !self.life.is_closed()
            && self
                .attachments
                .prepare(source)
                .is_some_and(|prepare| prepare.generation == generation)
    }
    pub(in crate::xiaomi::adapter) fn pending_subscriptions(
        &self,
        source: &NotificationSource,
        generation: u64,
    ) -> Option<Vec<SubscriptionBinding>> {
        let prepare = self.attachments.prepare(source)?;
        (!self.life.is_closed() && prepare.generation == generation)
            .then(|| prepare.missing(&self.subscriptions()))
    }
    pub(in crate::xiaomi::adapter) fn subscription_result(
        &self,
        source: &NotificationSource,
        generation: u64,
        binding: SubscriptionBinding,
        result: Result<(), String>,
    ) -> bool {
        let Some(mut prepare) = self.attachments.prepare_mut(source) else {
            return false;
        };
        if self.life.is_closed() || prepare.generation != generation {
            return false;
        }
        match result {
            Ok(()) => prepare.subscribed(binding),
            Err(problem) => {
                prepare.problems.insert(binding, problem);
            }
        }
        true
    }
    pub(in crate::xiaomi::adapter) fn finish_preparation(
        &self,
        source: &NotificationSource,
        generation: u64,
    ) {
        if let Some(mut prepare) = self.attachments.prepare_mut(source)
            && !self.life.is_closed()
            && prepare.generation == generation
        {
            prepare.finish();
        }
    }
    pub(in crate::xiaomi::adapter) fn abandon_preparation(
        &self,
        source: &NotificationSource,
        generation: u64,
    ) {
        if let Some(mut prepare) = self.attachments.prepare_mut(source)
            && !self.life.is_closed()
            && prepare.generation == generation
        {
            prepare.running = false;
        }
    }
    pub(in crate::xiaomi::adapter) fn function_subscription_problem(
        &self,
        id: &str,
    ) -> Option<String> {
        let local = self.attachments.local.borrow();
        let cloud = self.attachments.cloud.borrow();
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
    pub(in crate::xiaomi::adapter) fn source_ready(
        &self,
        id: &str,
        source: &NotificationSource,
    ) -> bool {
        let mut bindings = self.channel_subscriptions(id);
        if let Some((mode, _)) = self
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .and_then(ParsedFunction::relay_mode)
        {
            bindings.retain(|binding| *binding != SubscriptionBinding::Property(mode));
        }
        self.bindings_ready(source, &bindings)
    }

    pub(in crate::xiaomi::adapter) fn source_binding_ready(
        &self,
        source: &NotificationSource,
        binding: PropertyBinding,
    ) -> bool {
        self.bindings_ready(source, &[SubscriptionBinding::Property(binding)])
    }

    fn bindings_ready(
        &self,
        source: &NotificationSource,
        bindings: &[SubscriptionBinding],
    ) -> bool {
        !self.life.is_closed()
            && self
                .attachments
                .prepare(source)
                .is_some_and(|prepare| prepare.ready(bindings))
    }

    fn subscription_problems(&self) -> Vec<String> {
        let required = self
            .required_subscriptions()
            .into_iter()
            .collect::<BTreeSet<_>>();
        let local = self.attachments.local.borrow();
        let cloud = self.attachments.cloud.borrow();
        [&local.prepare, &cloud.prepare]
            .into_iter()
            .flat_map(|prepare| {
                required
                    .iter()
                    .filter_map(|binding| prepare.problems.get(binding).cloned())
            })
            .collect()
    }
    pub(in crate::xiaomi::adapter) fn pending_problems(&self) -> Vec<String> {
        let mut values = self.subscription_problems();
        values.extend(self.refresh.borrow().problems_except(BATTERY_CHANNEL));
        values
    }
    pub(in crate::xiaomi::adapter) fn battery_problem(&self) -> Option<String> {
        let mut values = Vec::new();
        if let Some(issue) = self.committed_battery_issue.borrow().as_ref() {
            values.push(issue.clone());
        }
        if let Some(binding) = self.battery_binding() {
            let local = self.attachments.local.borrow();
            let cloud = self.attachments.cloud.borrow();
            values.extend(
                [&local.prepare, &cloud.prepare]
                    .into_iter()
                    .filter_map(|prepare| prepare.problems.get(&binding.into()).cloned()),
            );
        }
        if let Some(problem) = self.battery_read_problem.borrow().clone() {
            values.push(problem);
        }
        (!values.is_empty()).then(|| values.join("; "))
    }
    pub(in crate::xiaomi::adapter) fn set_committed_battery_issue(&self, issue: Option<&str>) {
        *self.committed_battery_issue.borrow_mut() = issue.map(str::to_owned);
    }
    pub(in crate::xiaomi::adapter) fn set_battery_read_problem(&self, problem: Option<String>) {
        *self.battery_read_problem.borrow_mut() = problem;
    }
    pub(in crate::xiaomi::adapter) fn subscription_problem(&self) -> Option<String> {
        let values = self.subscription_problems();
        (!values.is_empty()).then(|| values.join("; "))
    }
    pub(in crate::xiaomi::adapter) fn report(&self, context: &Context) {
        if self.life.is_closed() {
            return;
        }
        context.status_changed.notify(usize::MAX);
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
    let source = NotificationSource::Local(handle.clone());
    loop {
        let (connection, preparation) = work.local_preparation_request().await;
        if !connection.same_connection(handle) {
            work.abandon_preparation(&source, preparation);
            continue;
        }
        if !context.current_local(handle) || !work.preparation_current(&source, preparation) {
            work.abandon_preparation(&source, preparation);
            continue;
        }
        context
            .clock
            .wait_until(context.clock.now() + Duration::from_millis(10))
            .await;
        let current =
            || context.current_local(handle) && work.preparation_current(&source, preparation);
        let Some(mut pending) = work.pending_subscriptions(&source, preparation) else {
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
                let receipt_source = source.clone();
                let on_result = move |result: Result<(), crate::xiaomi::central::CentralError>| {
                    if receipt_context.current_local(&receipt_handle)
                        && receipt_work.subscription_result(
                            &receipt_source,
                            preparation,
                            binding,
                            result.map_err(|error| {
                                format!("{} subscribe {binding}: {error}", receipt_work.key.id)
                            }),
                        )
                    {
                        receipt_context.changed.notify(usize::MAX);
                        receipt_work.report(&receipt_context);
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
                    Err(_) => failed.push(binding),
                }
            }
            if failed.is_empty() || attempt == 3 {
                work.finish_preparation(&source, preparation);
                work.report(context);
                break;
            }
            pending = failed;
            context
                .clock
                .wait_until(context.clock.now() + Duration::from_millis(100))
                .await;
        }
        work.abandon_preparation(&source, preparation);
        context.changed.notify(usize::MAX);
    }
}
