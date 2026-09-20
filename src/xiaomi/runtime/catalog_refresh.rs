use super::{AUTH_MAINTENANCE_INTERVAL, CATALOG_REFRESH_INTERVAL};
use crate::{
    storage::{AuthSnapshot, DeviceStore, StorageError},
    xiaomi::{
        catalog::assemble_catalog,
        cloud::{CloudClient, OwnedCatalog},
    },
};
use futures_util::{FutureExt, future::LocalBoxFuture};
use std::{
    collections::{BTreeSet, HashMap},
    rc::Rc,
    time::Instant,
};

pub(super) enum CatalogAction {
    None,
    Resolve {
        snapshot: Box<AuthSnapshot>,
        owned: OwnedCatalog,
    },
    Retry,
    Complete,
}

/// Owns the two-stage directory fetch and specification resolution task.
pub(super) struct CatalogRefresh {
    cloud: Rc<CloudClient>,
    devices: DeviceStore,
    task: Option<LocalBoxFuture<'static, CatalogTaskResult>>,
    next: Instant,
}

impl CatalogRefresh {
    pub(super) fn new(cloud: Rc<CloudClient>, devices: DeviceStore) -> Self {
        Self {
            cloud,
            devices,
            task: None,
            next: Instant::now(),
        }
    }
    pub(super) fn client(&self) -> &Rc<CloudClient> {
        &self.cloud
    }
    pub(super) fn due(&self, now: Instant, requested: bool) -> bool {
        self.task.is_none() && (now >= self.next || requested)
    }
    pub(super) fn start(&mut self, snapshot: AuthSnapshot) {
        self.task = Some(catalog_task(self.cloud.clone(), snapshot));
        self.next = Instant::now() + CATALOG_REFRESH_INTERVAL;
    }
    pub(super) fn cancel(&mut self) {
        self.task = None;
    }
    pub(super) fn reset(&mut self) {
        self.cancel();
        self.next = Instant::now();
    }
    pub(super) fn running(&self) -> bool {
        self.task.is_some()
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.task.is_none().then_some(self.next)
    }
    pub(super) async fn next_event(&mut self) -> CatalogTaskResult {
        match self.task.as_mut() {
            Some(task) => task.await,
            None => std::future::pending().await,
        }
    }
    pub(super) fn apply(&mut self, action: CatalogAction) {
        match action {
            CatalogAction::None => {}
            CatalogAction::Resolve { snapshot, owned } => {
                self.task = Some(catalog_specs_task(
                    self.cloud.clone(),
                    self.devices.clone(),
                    *snapshot,
                    owned,
                ));
            }
            CatalogAction::Retry => self.next = Instant::now() + AUTH_MAINTENANCE_INTERVAL,
            CatalogAction::Complete => self.next = Instant::now() + CATALOG_REFRESH_INTERVAL,
        }
    }
    #[cfg(test)]
    pub(super) fn set_client(&mut self, cloud: Rc<CloudClient>) {
        self.cloud = cloud;
    }
    #[cfg(test)]
    pub(super) fn defer_until(&mut self, next: Instant) {
        self.next = next;
    }
}

pub(super) enum CatalogTaskResult {
    Owned {
        snapshot: AuthSnapshot,
        owned: Option<crate::xiaomi::cloud::OwnedCatalog>,
    },
    Resolved {
        snapshot: AuthSnapshot,
        result: Result<
            (
                crate::xiaomi::catalog::DeviceCatalog,
                HashMap<String, String>,
            ),
            CatalogRefreshError,
        >,
    },
}

pub(super) enum CatalogRefreshError {
    Storage(StorageError),
    Remote,
}

fn catalog_task(
    cloud: Rc<CloudClient>,
    snapshot: AuthSnapshot,
) -> LocalBoxFuture<'static, CatalogTaskResult> {
    async move {
        let owned = match snapshot.record.as_ref() {
            Some(record) => cloud
                .get_owned_catalog_for_uid(&record.tokens.access_token, &record.uid)
                .await
                .ok(),
            None => None,
        };
        CatalogTaskResult::Owned { snapshot, owned }
    }
    .boxed_local()
}

fn catalog_specs_task(
    cloud: Rc<CloudClient>,
    devices: crate::storage::DeviceStore,
    snapshot: AuthSnapshot,
    owned: crate::xiaomi::cloud::OwnedCatalog,
) -> LocalBoxFuture<'static, CatalogTaskResult> {
    async move {
        let result = async {
            let types = owned
                .devices
                .iter()
                .filter_map(|device| device.spec_type.clone())
                .collect::<BTreeSet<_>>();
            let mut specifications = HashMap::new();
            for type_urn in types {
                let (document, cached) = match devices
                    .load_spec(&type_urn)
                    .map_err(CatalogRefreshError::Storage)?
                {
                    Some(cached) => (cached.document, true),
                    None => match cloud.get_spec_instance(&type_urn).await {
                        Ok(document) => (document, false),
                        Err(_) => continue,
                    },
                };
                let invalid = owned
                    .devices
                    .iter()
                    .filter(|device| device.spec_type.as_deref() == Some(type_urn.as_str()))
                    .find_map(|device| {
                        crate::xiaomi::catalog::compile_spec(&device.model, &document).err()
                    });
                if let Some(error) = invalid {
                    if cached {
                        return Err(CatalogRefreshError::Storage(StorageError::new(
                            devices.path(),
                            "compile cached Xiaomi device specification",
                            error,
                        )));
                    }
                    continue;
                }
                specifications.insert(type_urn, document);
            }
            // Specifications that failed independently remain absent. The catalog keeps
            // their ownership records with no controllable features.
            let catalog = assemble_catalog(&owned, &specifications)
                .map_err(|_| CatalogRefreshError::Remote)?;
            Ok((catalog, specifications))
        }
        .await;
        CatalogTaskResult::Resolved { snapshot, result }
    }
    .boxed_local()
}
