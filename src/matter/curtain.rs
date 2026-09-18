use crate::device::{
    Capability, CommandOutcome, DeviceCommand, DeviceService, FeatureIdentity, Percent, Property,
    PropertyState, PropertyValue,
};
use rs_matter::{
    dm::{Cluster, Dataver, ReadContext, WriteContext, clusters::decl::window_covering},
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};
use std::{cell::RefCell, future::Future};

pub(super) const CLUSTER: Cluster<'static> = window_covering::FULL_CLUSTER
    .with_features(
        window_covering::Feature::LIFT
            .union(window_covering::Feature::POSITION_AWARE_LIFT)
            .bits(),
    )
    .with_attrs(with!(
        required;
        window_covering::AttributeId::TargetPositionLiftPercent100ths
            | window_covering::AttributeId::CurrentPositionLiftPercent100ths
    ))
    .with_cmds(with!(
        window_covering::CommandId::UpOrOpen
            | window_covering::CommandId::DownOrClose
            | window_covering::CommandId::StopMotion
            | window_covering::CommandId::GoToLiftPercentage
    ))
    .with_events(with!());

pub(super) struct CurtainHandler {
    service: DeviceService,
    feature: FeatureIdentity,
    capabilities: RefCell<Vec<Capability>>,
    dataver: Dataver,
}

impl CurtainHandler {
    pub(super) fn new(
        service: DeviceService,
        feature: FeatureIdentity,
        capabilities: Vec<Capability>,
        seed: u32,
    ) -> Self {
        Self {
            service,
            feature,
            capabilities: RefCell::new(capabilities),
            dataver: Dataver::new(seed),
        }
    }

    pub(super) fn dataver(&self) -> &Dataver {
        &self.dataver
    }

    pub(super) fn set_capabilities(&self, capabilities: Vec<Capability>) {
        *self.capabilities.borrow_mut() = capabilities;
    }

    fn current(&self, property: Property) -> Option<PropertyValue> {
        self.service
            .snapshot(&self.feature)
            .and_then(|snapshot| snapshot.property(property).cloned())
            .and_then(|state| match state {
                PropertyState::Current { value, .. } => Some(value),
                PropertyState::LastKnown { .. } | PropertyState::Unknown { .. } => None,
            })
    }

    fn position(&self, property: Property) -> Option<Percent> {
        match self.current(property) {
            Some(PropertyValue::Percent(value)) => Some(value),
            _ => None,
        }
    }

    fn range(&self) -> Result<crate::device::NumericRange, Error> {
        self.capabilities
            .borrow()
            .iter()
            .find_map(|capability| match capability {
                Capability::CurtainPosition(range) => Some(*range),
                _ => None,
            })
            .ok_or_else(|| ErrorCode::Failure.into())
    }

    fn matter_position(value: Percent) -> u16 {
        ((100.0 - value.get()) * 100.0).round() as u16
    }

    async fn command(&self, command: DeviceCommand) -> Result<(), Error> {
        match self.service.command(&self.feature, command).await {
            CommandOutcome::Accepted => Ok(()),
            CommandOutcome::Unsupported | CommandOutcome::Rejected(_) => {
                Err(ErrorCode::ConstraintError.into())
            }
            _ => Err(ErrorCode::Failure.into()),
        }
    }

    async fn set_matter_position(&self, value: u16) -> Result<(), Error> {
        if value > 10_000 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let core = 100.0 - f64::from(value) / 100.0;
        if !self.range()?.accepts(core) {
            return Err(ErrorCode::ConstraintError.into());
        }
        self.command(DeviceCommand::SetCurtainPosition(
            Percent::new(core).map_err(|_| ErrorCode::ConstraintError)?,
        ))
        .await
    }
}

impl window_covering::ClusterAsyncHandler for CurtainHandler {
    const CLUSTER: Cluster<'static> = CLUSTER;

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    async fn r#type(&self, _ctx: impl ReadContext) -> Result<window_covering::Type, Error> {
        Ok(window_covering::Type::Unknown)
    }

    async fn config_status(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<window_covering::ConfigStatus, Error> {
        Ok(window_covering::ConfigStatus::OPERATIONAL
            | window_covering::ConfigStatus::LIFT_POSITION_AWARE)
    }

    async fn operational_status(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<window_covering::OperationalStatus, Error> {
        let value = match self.current(Property::CurtainMovement) {
            Some(PropertyValue::CurtainMovement(crate::device::CurtainMovement::Opening)) => 0x05,
            Some(PropertyValue::CurtainMovement(crate::device::CurtainMovement::Closing)) => 0x0a,
            Some(PropertyValue::CurtainMovement(crate::device::CurtainMovement::Stopped)) => 0,
            _ => return Err(ErrorCode::Failure.into()),
        };
        Ok(window_covering::OperationalStatus::from_bits_retain(value))
    }

    fn target_position_lift_percent_100_ths(
        &self,
        _ctx: impl ReadContext,
    ) -> impl Future<Output = Result<Nullable<u16>, Error>> {
        std::future::ready(Ok(self
            .position(Property::CurtainTargetPosition)
            .map(Self::matter_position)
            .into()))
    }

    async fn end_product_type(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<window_covering::EndProductType, Error> {
        Ok(window_covering::EndProductType::Unknown)
    }

    fn current_position_lift_percent_100_ths(
        &self,
        _ctx: impl ReadContext,
    ) -> impl Future<Output = Result<Nullable<u16>, Error>> {
        std::future::ready(Ok(self
            .position(Property::CurtainPosition)
            .map(Self::matter_position)
            .into()))
    }

    async fn mode(&self, _ctx: impl ReadContext) -> Result<window_covering::Mode, Error> {
        Ok(window_covering::Mode::empty())
    }

    async fn set_mode(
        &self,
        _ctx: impl WriteContext,
        value: window_covering::Mode,
    ) -> Result<(), Error> {
        if value.is_empty() {
            Ok(())
        } else {
            Err(ErrorCode::ConstraintError.into())
        }
    }

    async fn handle_up_or_open(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
    ) -> Result<(), Error> {
        self.set_matter_position(0).await
    }

    async fn handle_down_or_close(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
    ) -> Result<(), Error> {
        self.set_matter_position(10_000).await
    }

    async fn handle_stop_motion(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
    ) -> Result<(), Error> {
        self.service
            .stop_adjustment(&self.feature, Property::CurtainTargetPosition);
        self.command(DeviceCommand::StopCurtain).await
    }

    async fn handle_go_to_lift_value(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: window_covering::GoToLiftValueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_go_to_lift_percentage(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        request: window_covering::GoToLiftPercentageRequest<'_>,
    ) -> Result<(), Error> {
        self.set_matter_position(request.lift_percent_100_ths_value()?)
            .await
    }

    async fn handle_go_to_tilt_value(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: window_covering::GoToTiltValueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_go_to_tilt_percentage(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: window_covering::GoToTiltPercentageRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}
