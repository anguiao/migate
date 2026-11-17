//! Heating-only Matter thermostat projection.

use super::thermostat::{HeaterProfile, Thermostat};
use crate::device::Function;
use rs_matter::{
    dm::{
        AsyncHandler, Dataver, HandlerContext, InvokeContext, InvokeReply, MatchContext,
        ReadContext, ReadReply, WriteContext,
    },
    error::Error,
};

pub(super) type HeaterThermostat = Thermostat<HeaterProfile>;

pub(super) struct Heater {
    thermostat: HeaterThermostat,
}
impl Heater {
    pub(super) fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
        Self {
            thermostat: Thermostat::new(function, endpoint, dataver),
        }
    }
    pub(super) fn sync(&self) {
        self.thermostat.sync();
    }
    fn handler(&self) -> impl AsyncHandler + '_ {
        &self.thermostat
    }
}
impl AsyncHandler for Heater {
    fn bump_dataver(&self, _: impl MatchContext) {}
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        self.handler().read(ctx, reply).await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        self.handler().write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        self.handler().invoke(ctx, reply).await
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.handler().run(ctx).await
    }
}
