use crate::{
    device::{BatterySnapshot, PhysicalDevice},
    matter::units,
};
use rs_matter::{
    dm::{
        ArrayAttributeRead, Cluster, Dataver, HandlerContext, ReadContext,
        clusters::decl::power_source,
    },
    error::{Error, ErrorCode},
    im::EndptId,
    tlv::{Nullable, TLVBuilderParent, ToTLVArrayBuilder, ToTLVBuilder, Utf8StrBuilder},
    with,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BatteryProjection {
    percent: Option<u8>,
    charge: Option<power_source::BatChargeLevelEnum>,
    replacement: Option<bool>,
}
impl BatteryProjection {
    const UNKNOWN: Self = Self {
        percent: None,
        charge: None,
        replacement: None,
    };
    fn project(snapshot: BatterySnapshot) -> Self {
        let Some(percent) = snapshot.value else {
            return Self::UNKNOWN;
        };
        let Some(doubled) = units::battery_percent_matter_value(percent) else {
            return Self::UNKNOWN;
        };
        let (charge, replacement) = if percent > 10.0 {
            (power_source::BatChargeLevelEnum::OK, false)
        } else if percent > 5.0 {
            (power_source::BatChargeLevelEnum::Warning, false)
        } else {
            (power_source::BatChargeLevelEnum::Critical, true)
        };
        Self {
            percent: Some(doubled),
            charge: Some(charge),
            replacement: Some(replacement),
        }
    }
}

pub(in crate::matter) struct BatteryHandler {
    physical: PhysicalDevice,
    dataver: Dataver,
    endpoint_list: Vec<u16>,
    versioned: std::cell::Cell<BatteryProjection>,
    observed: std::cell::Cell<BatteryProjection>,
}
impl BatteryHandler {
    pub(in crate::matter) fn new(
        physical: PhysicalDevice,
        endpoint: u16,
        endpoint_list: Vec<u16>,
        dataver: Dataver,
    ) -> Result<Self, Error> {
        let snapshot = physical.battery_snapshot().ok_or(ErrorCode::InvalidState)?;
        let projection = BatteryProjection::project(snapshot);
        Ok(Self {
            physical,
            dataver,
            endpoint_list: std::iter::once(endpoint).chain(endpoint_list).collect(),
            versioned: std::cell::Cell::new(projection),
            observed: std::cell::Cell::new(projection),
        })
    }
    fn sync_version(&self) -> BatteryProjection {
        let current = self
            .physical
            .battery_snapshot()
            .map_or(BatteryProjection::UNKNOWN, BatteryProjection::project);
        if self.versioned.replace(current) != current {
            self.dataver.changed();
        }
        current
    }
}
impl power_source::ClusterHandler for BatteryHandler {
    const CLUSTER: Cluster<'static> = power_source::FULL_CLUSTER
        .with_features(power_source::Feature::BATTERY.bits())
        .with_attrs(|attribute, _, _| {
            matches!(
                attribute.id,
                0..=2 | 12 | 14..=16 | 31 | 0xfff8..=0xfffd
            )
        })
        .with_cmds(with!())
        .with_events(with!());

    fn dataver(&self) -> u32 {
        self.sync_version();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    fn status(&self, _ctx: impl ReadContext) -> Result<power_source::PowerSourceStatusEnum, Error> {
        Ok(power_source::PowerSourceStatusEnum::Unspecified)
    }
    fn order(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(0)
    }
    fn description<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        out: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        out.set("Battery")
    }
    fn bat_percent_remaining(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(self.sync_version().percent))
    }
    fn bat_charge_level(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<power_source::BatChargeLevelEnum, Error> {
        self.sync_version()
            .charge
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    fn bat_replacement_needed(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        self.sync_version()
            .replacement
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    fn bat_replaceability(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<power_source::BatReplaceabilityEnum, Error> {
        Ok(power_source::BatReplaceabilityEnum::Unspecified)
    }
    fn endpoint_list<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<ToTLVArrayBuilder<P, EndptId>, ToTLVBuilder<P, EndptId>>,
    ) -> Result<P, Error> {
        match builder {
            ArrayAttributeRead::ReadAll(mut builder) => {
                for endpoint in &self.endpoint_list {
                    builder = builder.push(endpoint)?;
                }
                builder.end()
            }
            ArrayAttributeRead::ReadOne(index, builder) => {
                let Some(endpoint) = self.endpoint_list.get(index as usize) else {
                    return Err(ErrorCode::ConstraintError.into());
                };
                builder.set(endpoint)
            }
            ArrayAttributeRead::ReadNone(builder) => builder.end(),
        }
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.physical.subscribe_battery();
        let mut observed = self.observed.get();
        loop {
            let current = self.sync_version();
            if observed != current {
                if observed.percent != current.percent {
                    ctx.notify_attr_changed(
                        self.physical_endpoint(),
                        Self::CLUSTER.id,
                        power_source::AttributeId::BatPercentRemaining as _,
                    );
                }
                if observed.charge != current.charge {
                    ctx.notify_attr_changed(
                        self.physical_endpoint(),
                        Self::CLUSTER.id,
                        power_source::AttributeId::BatChargeLevel as _,
                    );
                }
                if observed.replacement != current.replacement {
                    ctx.notify_attr_changed(
                        self.physical_endpoint(),
                        Self::CLUSTER.id,
                        power_source::AttributeId::BatReplacementNeeded as _,
                    );
                }
                observed = current;
                self.observed.set(current);
            }
            changes.changed().await;
        }
    }
}
impl BatteryHandler {
    fn physical_endpoint(&self) -> u16 {
        self.endpoint_list[0]
    }
}
