use super::{DeviceSessions, SessionContext};
use crate::xiaomi::runtime::{
    XiaomiFailureStage, XiaomiRuntimeComponent, XiaomiRuntimeError, XiaomiSafeFailureCode,
    catalog_refresh::{CatalogAction, CatalogRefreshError, CatalogTaskResult},
    unix_time,
};
use crate::{
    storage::StorageError,
    xiaomi::{
        catalog::{assemble_catalog, persist_catalog},
        runtime::{CloudEvidence, CloudStatus},
    },
};
use std::collections::HashMap;

impl DeviceSessions {
    pub(in crate::xiaomi::runtime) fn finish_catalog_refresh(
        &mut self,
        ctx: &SessionContext<'_>,
        task: CatalogTaskResult,
    ) -> Result<CatalogAction, XiaomiRuntimeError> {
        let task_snapshot = match &task {
            CatalogTaskResult::Owned { snapshot, .. }
            | CatalogTaskResult::Resolved { snapshot, .. } => snapshot,
        };
        let current = ctx.auth.observe()?;
        if current.revision != task_snapshot.revision
            || current.session_generation != task_snapshot.session_generation
        {
            return Ok(CatalogAction::None);
        }
        match task {
            CatalogTaskResult::Owned { snapshot, owned } => {
                let Some(owned) = owned else {
                    ctx.record_boundary(
                        XiaomiRuntimeComponent::Catalog,
                        XiaomiFailureStage::Read,
                        XiaomiSafeFailureCode::Unavailable,
                        None,
                    );
                    return Ok(CatalogAction::Retry);
                };
                let Some(record) = snapshot.record.as_ref() else {
                    return Ok(CatalogAction::None);
                };
                self.cloud_validated = true;
                let specifications = self
                    .catalog
                    .as_ref()
                    .map(|catalog| catalog.specifications.clone())
                    .unwrap_or_default();
                let mut catalog = match assemble_catalog(&owned, &HashMap::new()) {
                    Ok(catalog) => catalog,
                    Err(_) => {
                        ctx.record_boundary(
                            XiaomiRuntimeComponent::Catalog,
                            XiaomiFailureStage::Read,
                            XiaomiSafeFailureCode::Protocol,
                            None,
                        );
                        return Ok(CatalogAction::Retry);
                    }
                };
                if let Some(previous) = self.catalog.as_ref() {
                    for device in &mut catalog.devices {
                        if let Some(old) = previous.catalog.devices.iter().find(|old| {
                            old.home_id == device.home_id
                                && old.parent_did == device.parent_did
                                && old.model == device.model
                                && old.spec_type == device.spec_type
                        }) {
                            device.features = old.features.clone();
                        }
                    }
                }
                let admission_catalog = crate::xiaomi::runtime::AdmissionCatalog {
                    account: crate::device::AccountId::new(record.uid.clone()).map_err(
                        |error| {
                            StorageError::new(ctx.runner_path(), "validate Xiaomi account", error)
                        },
                    )?,
                    session_generation: snapshot.session_generation,
                    catalog,
                    specifications,
                };
                self.admission.apply_complete_catalog(&admission_catalog)?;
                let admission_snapshot = self.admission.snapshot()?;
                self.rebuild_gateway_routes(ctx, &admission_snapshot);
                self.schedule_gateway_operations();
                let session_generation = admission_catalog.session_generation;
                self.catalog = Some(admission_catalog);
                self.admission.observe_cloud(&CloudEvidence {
                    account: crate::device::AccountId::new(record.uid.clone()).map_err(
                        |error| {
                            StorageError::new(ctx.runner_path(), "validate Xiaomi account", error)
                        },
                    )?,
                    session_generation,
                    status: CloudStatus::Ready,
                })?;
                self.sync_cloud_routes(ctx)?;
                self.reconcile(ctx, &self.admission)?;
                self.update_status(ctx);
                return Ok(CatalogAction::Resolve {
                    snapshot: Box::new(snapshot),
                    owned,
                });
            }
            CatalogTaskResult::Resolved { snapshot, result } => {
                let (catalog, specifications) = match result {
                    Err(CatalogRefreshError::Storage(error)) => return Err(error.into()),
                    Err(CatalogRefreshError::Remote) => {
                        ctx.record_boundary(
                            XiaomiRuntimeComponent::Catalog,
                            XiaomiFailureStage::Read,
                            XiaomiSafeFailureCode::Unavailable,
                            None,
                        );
                        return Ok(CatalogAction::Retry);
                    }
                    Ok(result) => result,
                };
                let Some(record) = snapshot.record.as_ref() else {
                    return Ok(CatalogAction::None);
                };
                let admission_catalog = crate::xiaomi::runtime::AdmissionCatalog {
                    account: crate::device::AccountId::new(record.uid.clone()).map_err(
                        |error| {
                            StorageError::new(ctx.runner_path(), "validate Xiaomi account", error)
                        },
                    )?,
                    session_generation: snapshot.session_generation,
                    catalog,
                    specifications,
                };
                self.admission.apply_complete_catalog(&admission_catalog)?;
                let admission_snapshot = self.admission.snapshot()?;
                self.rebuild_gateway_routes(ctx, &admission_snapshot);
                self.sync_cloud_routes(ctx)?;
                ctx.state.reconcile(&admission_snapshot);
                ctx.status.borrow_mut().admission = admission_snapshot;
                self.schedule_gateway_operations();
                let persisted = persist_catalog(
                    &ctx.devices(),
                    &admission_catalog.catalog,
                    &admission_catalog.specifications,
                    unix_time(),
                    snapshot.revision,
                )?;
                if persisted {
                    self.catalog = Some(admission_catalog);
                    self.update_status(ctx);
                    return Ok(CatalogAction::Complete);
                }
            }
        }
        Ok(CatalogAction::None)
    }
}
