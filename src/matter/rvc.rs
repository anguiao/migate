use crate::device::{
    Capability, CommandOutcome, DeviceCommand, DeviceService, FeatureIdentity, Property,
    PropertyState, PropertyValue, VacuumCleanMode, VacuumOperationalState,
};
use rs_matter::{
    dm::{
        ArrayAttributeRead, Cluster, Dataver, HandlerContext, InvokeContext, ReadContext,
        clusters::decl::{globals, rvc_clean_mode, rvc_operational_state, rvc_run_mode},
    },
    error::{Error, ErrorCode},
    tlv::{Nullable, TLVBuilderParent},
    with,
};
use std::cell::RefCell;

const MODE_SUCCESS: u8 = 0;
const MODE_UNSUPPORTED: u8 = 1;
const MODE_FAILURE: u8 = 2;
const VENDOR_ERROR: u8 = 0x80;

pub(super) const RUN_CLUSTER: Cluster<'static> = rvc_run_mode::FULL_CLUSTER
    .with_attrs(with!(required))
    .with_cmds(with!(rvc_run_mode::CommandId::ChangeToMode))
    .with_events(with!());
pub(super) const CLEAN_CLUSTER: Cluster<'static> = rvc_clean_mode::FULL_CLUSTER
    .with_attrs(with!(required))
    .with_cmds(with!(rvc_clean_mode::CommandId::ChangeToMode))
    .with_events(with!());
pub(super) const OPERATIONAL_CLUSTER: Cluster<'static> = rvc_operational_state::FULL_CLUSTER
    .with_attrs(with!(required))
    .with_cmds(with!(rvc_operational_state::CommandId::GoHome))
    .with_events(with!(rvc_operational_state::EventId::OperationalError));

pub(super) const fn operational_cluster(has_dock: bool) -> Cluster<'static> {
    if has_dock {
        OPERATIONAL_CLUSTER
    } else {
        OPERATIONAL_CLUSTER.with_cmds(with!())
    }
}

pub(super) struct RvcHandler {
    service: DeviceService,
    feature: FeatureIdentity,
    capabilities: RefCell<Vec<Capability>>,
    endpoint: u16,
    run_dataver: Dataver,
    clean_dataver: Dataver,
    operational_dataver: Dataver,
    observed_fault: RefCell<Option<String>>,
}

impl RvcHandler {
    pub(super) fn new(
        service: DeviceService,
        feature: FeatureIdentity,
        capabilities: Vec<Capability>,
        endpoint: u16,
        seed: u32,
    ) -> Self {
        let observed_fault = service
            .snapshot(&feature)
            .and_then(|snapshot| snapshot.property(Property::VacuumFault).cloned())
            .and_then(|state| state.last_known().map(|known| known.value))
            .and_then(|value| match value {
                PropertyValue::VacuumFault(value) => Some(value),
                _ => None,
            });
        Self {
            service,
            feature,
            capabilities: RefCell::new(capabilities),
            endpoint,
            run_dataver: Dataver::new(seed),
            clean_dataver: Dataver::new(seed.wrapping_add(1)),
            operational_dataver: Dataver::new(seed.wrapping_add(2)),
            observed_fault: RefCell::new(observed_fault),
        }
    }

    pub(super) fn set_capabilities(&self, capabilities: Vec<Capability>) {
        *self.capabilities.borrow_mut() = capabilities;
    }

    pub(super) fn dataver(&self, cluster: u32) -> Option<&Dataver> {
        match cluster {
            id if id == RUN_CLUSTER.id => Some(&self.run_dataver),
            id if id == CLEAN_CLUSTER.id => Some(&self.clean_dataver),
            id if id == OPERATIONAL_CLUSTER.id => Some(&self.operational_dataver),
            _ => None,
        }
    }

    fn current(&self, property: Property) -> Option<PropertyValue> {
        current(&self.service, &self.feature, property)
    }

    fn state(&self) -> Option<VacuumOperationalState> {
        match self.current(Property::VacuumOperationalState) {
            Some(PropertyValue::VacuumOperationalState(value)) => Some(value),
            _ => None,
        }
    }

    fn fault(&self) -> Option<i64> {
        match self.current(Property::VacuumFault) {
            Some(PropertyValue::VacuumFault(value)) => value.parse().ok(),
            _ => None,
        }
    }

    fn clean_modes(&self) -> Vec<VacuumCleanMode> {
        self.capabilities
            .borrow()
            .iter()
            .find_map(|capability| match capability {
                Capability::VacuumCleanModes(values) => Some(values.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn clean_mode(&self) -> Option<VacuumCleanMode> {
        match self.current(Property::VacuumCleanMode) {
            Some(PropertyValue::VacuumCleanMode(value)) => Some(value),
            _ => None,
        }
    }

    fn has_dock(&self) -> bool {
        self.capabilities.borrow().contains(&Capability::VacuumDock)
    }

    async fn command(&self, command: DeviceCommand) -> CommandOutcome {
        self.service.command(&self.feature, command).await
    }

    fn mode_status(outcome: CommandOutcome) -> u8 {
        match outcome {
            CommandOutcome::Accepted => MODE_SUCCESS,
            CommandOutcome::Unsupported => MODE_UNSUPPORTED,
            CommandOutcome::Rejected(_) => MODE_FAILURE,
            CommandOutcome::Unavailable
            | CommandOutcome::Expired
            | CommandOutcome::Cancelled
            | CommandOutcome::Superseded
            | CommandOutcome::Ambiguous => MODE_FAILURE,
        }
    }

    pub(super) fn observe_fault(&self, ctx: &impl HandlerContext) -> Result<(), Error> {
        let current = match self.current(Property::VacuumFault) {
            Some(PropertyValue::VacuumFault(value)) => value,
            _ => return Ok(()),
        };
        if self.observed_fault.borrow().as_deref() == Some(current.as_str()) {
            return Ok(());
        }
        *self.observed_fault.borrow_mut() = Some(current.clone());
        let Some(code) = current.parse::<i64>().ok() else {
            return Ok(());
        };
        if code == 0 {
            return Ok(());
        }
        let details = format!("MIoT fault {code}");
        rvc_operational_state::OperationalError::emit_for(ctx, self.endpoint, |builder| {
            builder
                .error_state()?
                .error_state_id(VENDOR_ERROR)?
                .error_state_label(Some("Vendor fault"))?
                .error_state_details(Some(&details))?
                .end()?
                .end()
        })?;
        Ok(())
    }
}

fn current(
    service: &DeviceService,
    feature: &FeatureIdentity,
    property: Property,
) -> Option<PropertyValue> {
    service
        .snapshot(feature)
        .and_then(|snapshot| snapshot.property(property).cloned())
        .and_then(|state| match state {
            PropertyState::Current { value, .. } => Some(value),
            PropertyState::LastKnown { .. } | PropertyState::Unknown { .. } => None,
        })
}

fn clean_mode_id(mode: VacuumCleanMode) -> u8 {
    match mode {
        VacuumCleanMode::Vacuum => 0,
        VacuumCleanMode::VacuumAndMop => 1,
        VacuumCleanMode::Mop => 2,
    }
}

fn clean_mode_for_id(mode: u8) -> Option<VacuumCleanMode> {
    match mode {
        0 => Some(VacuumCleanMode::Vacuum),
        1 => Some(VacuumCleanMode::VacuumAndMop),
        2 => Some(VacuumCleanMode::Mop),
        _ => None,
    }
}

fn clean_mode_option(mode: VacuumCleanMode) -> (&'static str, u8, &'static [u16]) {
    match mode {
        VacuumCleanMode::Vacuum => ("Vacuum", 0, &[rvc_clean_mode::ModeTag::Vacuum as u16]),
        VacuumCleanMode::VacuumAndMop => (
            "Vacuum and mop",
            1,
            &[
                rvc_clean_mode::ModeTag::Vacuum as u16,
                rvc_clean_mode::ModeTag::Mop as u16,
            ],
        ),
        VacuumCleanMode::Mop => ("Mop", 2, &[rvc_clean_mode::ModeTag::Mop as u16]),
    }
}

fn write_mode_option<P: TLVBuilderParent>(
    builder: globals::ModeOptionStructBuilder<P>,
    label: &str,
    mode: u8,
    tags: &[u16],
) -> Result<P, Error> {
    let mut values = builder.label(label)?.mode(mode)?.mode_tags()?;
    for tag in tags {
        values = values.push()?.mfg_code(None)?.value(*tag)?.end()?;
    }
    values.end()?.end()
}

fn error_response<P: TLVBuilderParent>(
    response: rvc_operational_state::OperationalCommandResponseBuilder<P>,
    error: u8,
) -> Result<P, Error> {
    response
        .command_response_state()?
        .error_state_id(error)?
        .error_state_label(None)?
        .error_state_details(None)?
        .end()?
        .end()
}

impl rvc_run_mode::ClusterAsyncHandler for RvcHandler {
    const CLUSTER: Cluster<'static> = RUN_CLUSTER;

    fn dataver(&self) -> u32 {
        self.run_dataver.get()
    }

    fn dataver_changed(&self) {
        self.run_dataver.changed();
    }

    async fn supported_modes<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<
            globals::ModeOptionStructArrayBuilder<P>,
            globals::ModeOptionStructBuilder<P>,
        >,
    ) -> Result<P, Error> {
        let options = [
            ("Idle", 0, &[rvc_run_mode::ModeTag::Idle as u16][..]),
            ("Cleaning", 1, &[rvc_run_mode::ModeTag::Cleaning as u16][..]),
        ];
        write_modes(builder, &options)
    }

    async fn current_mode(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        match self.state() {
            Some(VacuumOperationalState::Cleaning | VacuumOperationalState::Paused) => Ok(1),
            Some(VacuumOperationalState::Idle) => Ok(0),
            Some(
                VacuumOperationalState::Returning
                | VacuumOperationalState::Charging
                | VacuumOperationalState::Docked
                | VacuumOperationalState::Error,
            )
            // These states do not reveal whether a cleaning cycle is still active.
            | None => Err(ErrorCode::Failure.into()),
        }
    }

    async fn handle_change_to_mode<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: rvc_run_mode::ChangeToModeRequest<'_>,
        response: rvc_run_mode::ChangeToModeResponseBuilder<P>,
    ) -> Result<P, Error> {
        let status = match request.new_mode()? {
            0 => Self::mode_status(self.command(DeviceCommand::StopVacuum).await),
            1 => Self::mode_status(self.command(DeviceCommand::StartVacuum).await),
            _ => MODE_UNSUPPORTED,
        };
        response
            .status(status)?
            .status_text((status != MODE_SUCCESS).then_some("Mode change failed"))?
            .end()
    }
}

impl rvc_clean_mode::ClusterAsyncHandler for RvcHandler {
    const CLUSTER: Cluster<'static> = CLEAN_CLUSTER;

    fn dataver(&self) -> u32 {
        self.clean_dataver.get()
    }

    fn dataver_changed(&self) {
        self.clean_dataver.changed();
    }

    async fn supported_modes<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<
            globals::ModeOptionStructArrayBuilder<P>,
            globals::ModeOptionStructBuilder<P>,
        >,
    ) -> Result<P, Error> {
        let options = self
            .clean_modes()
            .into_iter()
            .map(clean_mode_option)
            .collect::<Vec<_>>();
        write_modes(builder, &options)
    }

    async fn current_mode(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        let mode = self.clean_mode().ok_or(ErrorCode::Failure)?;
        self.clean_modes()
            .contains(&mode)
            .then(|| clean_mode_id(mode))
            .ok_or_else(|| ErrorCode::Failure.into())
    }

    async fn handle_change_to_mode<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: rvc_clean_mode::ChangeToModeRequest<'_>,
        response: rvc_clean_mode::ChangeToModeResponseBuilder<P>,
    ) -> Result<P, Error> {
        let status = if let Some(mode) = clean_mode_for_id(request.new_mode()?) {
            if self.clean_modes().contains(&mode) {
                Self::mode_status(self.command(DeviceCommand::SetVacuumCleanMode(mode)).await)
            } else {
                MODE_UNSUPPORTED
            }
        } else {
            MODE_UNSUPPORTED
        };
        response
            .status(status)?
            .status_text((status != MODE_SUCCESS).then_some("Mode change failed"))?
            .end()
    }
}

fn write_modes<P: TLVBuilderParent>(
    builder: ArrayAttributeRead<
        globals::ModeOptionStructArrayBuilder<P>,
        globals::ModeOptionStructBuilder<P>,
    >,
    modes: &[(&str, u8, &[u16])],
) -> Result<P, Error> {
    match builder {
        ArrayAttributeRead::ReadAll(mut array) => {
            for (label, mode, tags) in modes {
                array = write_mode_option(array.push()?, label, *mode, tags)?;
            }
            array.end()
        }
        ArrayAttributeRead::ReadOne(index, builder) => {
            let (label, mode, tags) = modes
                .get(index as usize)
                .ok_or(ErrorCode::ConstraintError)?;
            write_mode_option(builder, label, *mode, tags)
        }
        ArrayAttributeRead::ReadNone(array) => array.end(),
    }
}

impl rvc_operational_state::ClusterAsyncHandler for RvcHandler {
    const CLUSTER: Cluster<'static> = OPERATIONAL_CLUSTER;

    fn dataver(&self) -> u32 {
        self.operational_dataver.get()
    }

    fn dataver_changed(&self) {
        self.operational_dataver.changed();
    }

    async fn phase_list<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<
            rs_matter::tlv::NullableBuilder<P, rs_matter::tlv::Utf8StrArrayBuilder<P>>,
            rs_matter::tlv::Utf8StrBuilder<P>,
        >,
    ) -> Result<P, Error> {
        match builder {
            ArrayAttributeRead::ReadAll(builder) => builder.null(),
            ArrayAttributeRead::ReadOne(_, _) => Err(ErrorCode::ConstraintError.into()),
            ArrayAttributeRead::ReadNone(builder) => builder.null(),
        }
    }

    async fn current_phase(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::none())
    }

    async fn operational_state_list<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<
            globals::OperationalStateStructArrayBuilder<P>,
            globals::OperationalStateStructBuilder<P>,
        >,
    ) -> Result<P, Error> {
        let states = [
            rvc_operational_state::OperationalStateEnum::Stopped as u8,
            rvc_operational_state::OperationalStateEnum::Running as u8,
            rvc_operational_state::OperationalStateEnum::Paused as u8,
            rvc_operational_state::OperationalStateEnum::VError as u8,
            rvc_operational_state::OperationalStateEnum::SeekingCharger as u8,
            rvc_operational_state::OperationalStateEnum::Charging as u8,
            rvc_operational_state::OperationalStateEnum::Docked as u8,
        ];
        match builder {
            ArrayAttributeRead::ReadAll(mut array) => {
                for state in states {
                    array = array
                        .push()?
                        .operational_state_id(state)?
                        .operational_state_label(None)?
                        .end()?;
                }
                array.end()
            }
            ArrayAttributeRead::ReadOne(index, builder) => builder
                .operational_state_id(
                    *states
                        .get(index as usize)
                        .ok_or(ErrorCode::ConstraintError)?,
                )?
                .operational_state_label(None)?
                .end(),
            ArrayAttributeRead::ReadNone(array) => array.end(),
        }
    }

    async fn operational_state(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        if self.fault().is_some_and(|fault| fault != 0) {
            return Ok(rvc_operational_state::OperationalStateEnum::VError as u8);
        }
        let state = match self.state() {
            Some(VacuumOperationalState::Idle) => {
                rvc_operational_state::OperationalStateEnum::Stopped
            }
            Some(VacuumOperationalState::Cleaning) => {
                rvc_operational_state::OperationalStateEnum::Running
            }
            Some(VacuumOperationalState::Paused) => {
                rvc_operational_state::OperationalStateEnum::Paused
            }
            Some(VacuumOperationalState::Returning) => {
                rvc_operational_state::OperationalStateEnum::SeekingCharger
            }
            Some(VacuumOperationalState::Charging) => {
                rvc_operational_state::OperationalStateEnum::Charging
            }
            Some(VacuumOperationalState::Docked) => {
                rvc_operational_state::OperationalStateEnum::Docked
            }
            Some(VacuumOperationalState::Error) => {
                rvc_operational_state::OperationalStateEnum::VError
            }
            None => return Err(ErrorCode::Failure.into()),
        };
        Ok(state as u8)
    }

    async fn operational_error<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: globals::ErrorStateStructBuilder<P>,
    ) -> Result<P, Error> {
        let fault = self.fault().ok_or(ErrorCode::Failure)?;
        if fault == 0 {
            builder
                .error_state_id(rvc_operational_state::ErrorStateEnum::NoError as u8)?
                .error_state_label(None)?
                .error_state_details(None)?
                .end()
        } else {
            let details = format!("MIoT fault {fault}");
            builder
                .error_state_id(VENDOR_ERROR)?
                .error_state_label(Some("Vendor fault"))?
                .error_state_details(Some(&details))?
                .end()
        }
    }

    async fn handle_pause<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        response: rvc_operational_state::OperationalCommandResponseBuilder<P>,
    ) -> Result<P, Error> {
        error_response(
            response,
            rvc_operational_state::ErrorStateEnum::CommandInvalidInState as u8,
        )
    }

    async fn handle_resume<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        response: rvc_operational_state::OperationalCommandResponseBuilder<P>,
    ) -> Result<P, Error> {
        error_response(
            response,
            rvc_operational_state::ErrorStateEnum::CommandInvalidInState as u8,
        )
    }

    async fn handle_go_home<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        response: rvc_operational_state::OperationalCommandResponseBuilder<P>,
    ) -> Result<P, Error> {
        let invalid = rvc_operational_state::ErrorStateEnum::CommandInvalidInState as u8;
        let success = rvc_operational_state::ErrorStateEnum::NoError as u8;
        let status = match (self.fault(), self.state()) {
            (Some(fault), _) if fault != 0 => invalid,
            (_, Some(VacuumOperationalState::Returning)) => success,
            (_, Some(VacuumOperationalState::Charging | VacuumOperationalState::Docked)) => invalid,
            (_, Some(VacuumOperationalState::Error) | None) => invalid,
            (
                _,
                Some(
                    VacuumOperationalState::Idle
                    | VacuumOperationalState::Cleaning
                    | VacuumOperationalState::Paused,
                ),
            ) if self.has_dock() => match self.command(DeviceCommand::ReturnVacuumToDock).await {
                CommandOutcome::Accepted => success,
                _ => rvc_operational_state::ErrorStateEnum::UnableToStartOrResume as u8,
            },
            _ => invalid,
        };
        error_response(response, status)
    }
}
