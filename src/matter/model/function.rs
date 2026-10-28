use super::*;

pub(super) struct FunctionHandler {
    groups: Dataver,
    identify: identify::IdentifyHandler,
    pub(super) kind: FunctionHandlerKind,
}
pub(super) enum FunctionHandlerKind {
    Outlet(OnOffHandler),
    Curtain,
    Light(Box<lighting::Lighting>),
    Temperature(sensors::TemperatureHandler),
    Humidity(sensors::HumidityHandler),
    Illuminance(sensors::IlluminanceHandler),
    Co2(sensors::Co2Sensor),
    Contact(states::ContactHandler),
    Occupancy(states::OccupancyHandler),
}
impl FunctionHandler {
    pub(super) fn new(function: Function, endpoint: u16, versions: Versions) -> Self {
        let kind = match function.description().kind {
            FunctionKind::Contact => FunctionHandlerKind::Contact(states::ContactHandler::new(
                function,
                endpoint,
                Dataver::new(versions.state),
            )),
            FunctionKind::Motion | FunctionKind::Occupancy => FunctionHandlerKind::Occupancy(
                states::OccupancyHandler::new(function, endpoint, Dataver::new(versions.state)),
            ),
            FunctionKind::Light(_) => FunctionHandlerKind::Light(Box::new(
                lighting::Lighting::new(
                    function,
                    endpoint,
                    Dataver::new(versions.on_off),
                    Dataver::new(versions.level),
                    Dataver::new(versions.color),
                )
                .expect("validated published lighting capabilities"),
            )),
            FunctionKind::Temperature(range) => {
                FunctionHandlerKind::Temperature(sensors::TemperatureHandler::new(
                    function,
                    range,
                    endpoint,
                    Dataver::new(versions.temperature),
                ))
            }
            FunctionKind::Humidity(range) => {
                FunctionHandlerKind::Humidity(sensors::HumidityHandler::new(
                    function,
                    range,
                    endpoint,
                    Dataver::new(versions.humidity),
                ))
            }
            FunctionKind::Illuminance(range) => {
                FunctionHandlerKind::Illuminance(sensors::IlluminanceHandler::new_illuminance(
                    function,
                    range,
                    endpoint,
                    Dataver::new(versions.illuminance),
                ))
            }
            FunctionKind::CarbonDioxide(range) => {
                FunctionHandlerKind::Co2(sensors::Co2Sensor::new(
                    function,
                    range,
                    endpoint,
                    Dataver::new(versions.co2),
                    Dataver::new(versions.air_quality),
                ))
            }
            FunctionKind::Outlet => FunctionHandlerKind::Outlet(OnOffHandler::new(
                function,
                endpoint,
                Dataver::new(versions.on_off),
            )),
            FunctionKind::Curtain => FunctionHandlerKind::Curtain,
        };
        Self {
            groups: Dataver::new(versions.groups),
            identify: identify::IdentifyHandler::new(Dataver::new(versions.identify)),
            kind,
        }
    }
    pub(super) fn light(&self) -> Option<&lighting::Lighting> {
        match &self.kind {
            FunctionHandlerKind::Light(light) => Some(light),
            FunctionHandlerKind::Outlet(_)
            | FunctionHandlerKind::Curtain
            | FunctionHandlerKind::Temperature(_)
            | FunctionHandlerKind::Humidity(_)
            | FunctionHandlerKind::Co2(_)
            | FunctionHandlerKind::Illuminance(_)
            | FunctionHandlerKind::Contact(_)
            | FunctionHandlerKind::Occupancy(_) => None,
        }
    }
    pub(super) fn is_sensor(&self) -> bool {
        matches!(
            self.kind,
            FunctionHandlerKind::Temperature(_)
                | FunctionHandlerKind::Humidity(_)
                | FunctionHandlerKind::Co2(_)
                | FunctionHandlerKind::Illuminance(_)
                | FunctionHandlerKind::Contact(_)
                | FunctionHandlerKind::Occupancy(_)
        )
    }
    pub(super) fn bump(&self, cluster: Option<u32>) {
        if !self.is_sensor() && cluster.is_none_or(|id| id == 4) {
            self.groups.changed();
        }
        if cluster.is_none_or(|id| id == 3) {
            self.identify.dataver_changed();
        }
        match &self.kind {
            FunctionHandlerKind::Outlet(handler)
                if cluster.is_none_or(|id| id == 6) =>
            {
                handler.dataver_changed()
            }
            FunctionHandlerKind::Temperature(handler)
                if cluster.is_none_or(|id| id == 0x402) =>
            {
                <sensors::TemperatureHandler as
                    temperature_measurement::ClusterHandler>::dataver_changed(handler)
            }
            FunctionHandlerKind::Humidity(handler)
                if cluster.is_none_or(|id| id == 0x405) =>
            {
                <sensors::HumidityHandler as
                    relative_humidity_measurement::ClusterHandler>::dataver_changed(handler)
            }
            FunctionHandlerKind::Illuminance(handler) if cluster.is_none_or(|id| id == 0x400) => {
                <sensors::IlluminanceHandler as illuminance_measurement::ClusterHandler>::dataver_changed(handler)
            }
            FunctionHandlerKind::Contact(handler) if cluster.is_none_or(|id| id == 0x45) => <states::ContactHandler as boolean_state::ClusterHandler>::dataver_changed(handler),
            FunctionHandlerKind::Occupancy(handler) if cluster.is_none_or(|id| id == 0x406) => <states::OccupancyHandler as occupancy_sensing::ClusterHandler>::dataver_changed(handler),
            FunctionHandlerKind::Co2(handler) => handler.bump(cluster),
            _ => {}
        }
        if let Some(light) = self.light() {
            light.bump_version(cluster);
        }
    }
    fn handler(&self) -> impl AsyncHandler + '_ {
        let matcher: fn(u16, u32) -> bool = match &self.kind {
            FunctionHandlerKind::Contact(_) => |_, cluster| cluster == 0x45,
            FunctionHandlerKind::Occupancy(_) => |_, cluster| cluster == 0x406,
            FunctionHandlerKind::Outlet(_) => |_, cluster| cluster == 6,
            FunctionHandlerKind::Curtain => |_, _| false,
            FunctionHandlerKind::Light(_) => |_, cluster| matches!(cluster, 6 | 8 | 0x300),
            FunctionHandlerKind::Temperature(_) => |_, cluster| cluster == 0x402,
            FunctionHandlerKind::Humidity(_) => |_, cluster| cluster == 0x405,
            FunctionHandlerKind::Illuminance(_) => |_, cluster| cluster == 0x400,
            FunctionHandlerKind::Co2(_) => |_, cluster| matches!(cluster, 0x040d | 0x005b),
        };
        let groups_matcher: fn(u16, u32) -> bool = if self.is_sensor() {
            |_, _| false
        } else {
            |_, cluster| cluster == 4
        };
        EmptyHandler
            .chain(
                groups_matcher,
                Async(
                    groups::GroupsHandler::new_with_identify(
                        Dataver::new(self.groups.get()),
                        &self.identify,
                    )
                    .adapt(),
                ),
            )
            .chain(
                |_, cluster| cluster == 3,
                Async(identify::HandlerAdaptor(&self.identify)),
            )
            .chain(matcher, &self.kind)
    }
}
impl AsyncHandler for FunctionHandler {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        self.handler().read(ctx, reply).await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        self.handler().write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if self.light().is_some()
            && ctx.cmd().cluster_id == 3
            && lighting::IDENTIFY.command(ctx.cmd().cmd_id).is_none()
        {
            return Err(ErrorCode::CommandNotFound.into());
        }
        self.handler().invoke(ctx, reply).await
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.handler().run(ctx).await
    }
}
macro_rules! dispatch_function {
    ($kind:expr, $method:ident($($arg:expr),* $(,)?)) => {
        match $kind {
            FunctionHandlerKind::Contact(handler) => AsyncHandler::$method(&Async(boolean_state::HandlerAdaptor(handler)), $($arg),*).await,
            FunctionHandlerKind::Occupancy(handler) => AsyncHandler::$method(&Async(occupancy_sensing::HandlerAdaptor(handler)), $($arg),*).await,
            FunctionHandlerKind::Outlet(handler) => AsyncHandler::$method(
                &on_off::HandlerAsyncAdaptor(handler), $($arg),*
            ).await,
            FunctionHandlerKind::Curtain => AsyncHandler::$method(&EmptyHandler, $($arg),*).await,
            FunctionHandlerKind::Light(light) => AsyncHandler::$method(
                &lighting::Dispatch(light), $($arg),*
            ).await,
            FunctionHandlerKind::Temperature(handler) => AsyncHandler::$method(
                &Async(temperature_measurement::HandlerAdaptor(handler)), $($arg),*
            ).await,
            FunctionHandlerKind::Humidity(handler) => AsyncHandler::$method(
                &Async(relative_humidity_measurement::HandlerAdaptor(handler)), $($arg),*
            ).await,
            FunctionHandlerKind::Illuminance(handler) => AsyncHandler::$method(
                &Async(illuminance_measurement::HandlerAdaptor(handler)), $($arg),*
            ).await,
            FunctionHandlerKind::Co2(handler) => AsyncHandler::$method(handler, $($arg),*).await,
        }
    };
}
impl AsyncHandler for FunctionHandlerKind {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        dispatch_function!(self, read(ctx, reply))
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        dispatch_function!(self, write(ctx))
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        dispatch_function!(self, invoke(ctx, reply))
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        dispatch_function!(self, run(ctx))
    }
}
