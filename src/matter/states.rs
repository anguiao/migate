//! Read-only binary-state projection; unknown native state is a failed read.
use crate::device::Function;
use rs_matter::{
    dm::{
        Cluster, Dataver, HandlerContext, ReadContext,
        clusters::decl::{boolean_state, occupancy_sensing},
    },
    error::{Error, ErrorCode},
    with,
};
use std::cell::Cell;

pub(super) type ContactHandler = StateHandler<true>;
pub(super) type OccupancyHandler = StateHandler<false>;

pub(super) struct StateHandler<const CONTACT: bool> {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    versioned: Cell<Option<bool>>,
    observed: Cell<Option<bool>>,
}
impl<const CONTACT: bool> StateHandler<CONTACT> {
    pub(super) fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
        let active = function.snapshot().state_sensor().unwrap().active;
        Self {
            function,
            endpoint,
            dataver,
            versioned: Cell::new(active),
            observed: Cell::new(active),
        }
    }
    fn sync_version(&self) -> Option<bool> {
        let active = self.function.snapshot().state_sensor().unwrap().active;
        if self.versioned.replace(active) != active {
            self.dataver.changed();
        }
        active
    }
    async fn run_state(&self, ctx: impl HandlerContext, cluster: u32) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        loop {
            let active = self.sync_version();
            if self.observed.replace(active) != active {
                ctx.notify_attr_changed(self.endpoint, cluster, 0);
            }
            changes.changed().await;
        }
    }
}
impl boolean_state::ClusterHandler for ContactHandler {
    const CLUSTER: Cluster<'static> = boolean_state::FULL_CLUSTER
        .with_features(0)
        .with_cmds(with!())
        .with_events(with!());
    fn dataver(&self) -> u32 {
        self.sync_version();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    fn state_value(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        self.sync_version()
            .map(|active| !active)
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.run_state(ctx, Self::CLUSTER.id).await
    }
}
impl occupancy_sensing::ClusterHandler for OccupancyHandler {
    const CLUSTER: Cluster<'static> = occupancy_sensing::FULL_CLUSTER
        .with_features(occupancy_sensing::Feature::OTHER.bits())
        .with_attrs(|attribute, _, _| matches!(attribute.id, 0..=2 | 0xfff8..=0xfffd))
        .with_cmds(with!())
        .with_events(with!());
    fn dataver(&self) -> u32 {
        self.sync_version();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    fn occupancy(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<occupancy_sensing::OccupancyBitmap, Error> {
        self.sync_version()
            .map(|active| {
                if active {
                    occupancy_sensing::OccupancyBitmap::OCCUPIED
                } else {
                    occupancy_sensing::OccupancyBitmap::empty()
                }
            })
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    fn occupancy_sensor_type(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<occupancy_sensing::OccupancySensorTypeEnum, Error> {
        Ok(occupancy_sensing::OccupancySensorTypeEnum::PIR)
    }
    fn occupancy_sensor_type_bitmap(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<occupancy_sensing::OccupancySensorTypeBitmap, Error> {
        Ok(occupancy_sensing::OccupancySensorTypeBitmap::PIR)
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.run_state(ctx, Self::CLUSTER.id).await
    }
}
