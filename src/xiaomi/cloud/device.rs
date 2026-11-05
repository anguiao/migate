use super::{
    CloudClient, CloudDevice, CloudDeviceCatalog, CloudDeviceInfo, CloudError, OwnedHome,
    OwnedHomes, catalog::HomeApi, light_groups::CatalogApi, properties::PropertyApi,
};
use crate::xiaomi::{
    capability::PropertyBinding,
    property::{PropertyReading, PropertyResults},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, value::RawValue};
use std::time::Duration;

pub(in crate::xiaomi) trait DeviceTransport {
    type Credential: ?Sized;

    fn read_property_path(&self) -> &'static str;
    fn write_property_path(&self) -> &'static str;

    async fn protected_post<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        path: &str,
        credential: &Self::Credential,
        body: Value,
    ) -> Result<T, CloudError> {
        self.protected_post_with_timeout(operation, path, credential, body, Duration::from_secs(30))
            .await
    }

    async fn protected_post_with_timeout<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        path: &str,
        credential: &Self::Credential,
        body: Value,
        timeout: Duration,
    ) -> Result<T, CloudError>;

    async fn protected_post_raw(
        &self,
        operation: &'static str,
        path: &str,
        credential: &Self::Credential,
        body: Value,
    ) -> Result<Box<RawValue>, CloudError>;
}

impl DeviceTransport for CloudClient {
    type Credential = str;

    fn read_property_path(&self) -> &'static str {
        "/app/v2/miotspec/prop/get"
    }
    fn write_property_path(&self) -> &'static str {
        "/app/v2/miotspec/prop/set"
    }

    async fn protected_post_with_timeout<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        path: &str,
        credential: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<T, CloudError> {
        CloudClient::protected_post_with_timeout(self, operation, path, credential, body, timeout)
            .await
    }

    async fn protected_post_raw(
        &self,
        operation: &'static str,
        path: &str,
        credential: &str,
        body: Value,
    ) -> Result<Box<RawValue>, CloudError> {
        CloudClient::protected_post_raw(self, operation, path, credential, body).await
    }
}

macro_rules! impl_device_api {
    ($client:ty, $credential:ty) => {
        impl $client {
            pub async fn get_owned_homes(
                &self,
                credential: &$credential,
            ) -> Result<OwnedHomes, CloudError> {
                HomeApi::get_owned_homes(self, credential).await
            }
            pub async fn get_home_devices(
                &self,
                credential: &$credential,
                home: &OwnedHome,
                requested_dids: &[String],
            ) -> Result<Vec<CloudDeviceInfo>, CloudError> {
                HomeApi::get_home_devices(self, credential, home, requested_dids).await
            }
            pub async fn get_device(
                &self,
                credential: &$credential,
                home: &OwnedHome,
                did: &str,
            ) -> Result<Option<CloudDevice>, CloudError> {
                HomeApi::get_device(self, credential, home, did).await
            }
            pub async fn get_device_catalog(
                &self,
                credential: &$credential,
            ) -> Result<CloudDeviceCatalog, CloudError> {
                CatalogApi::get_device_catalog(self, credential).await
            }
            pub async fn read_property_value(
                &self,
                credential: &$credential,
                did: &str,
                binding: PropertyBinding,
            ) -> Result<Value, CloudError> {
                PropertyApi::read_property_value(self, credential, did, binding).await
            }
            pub async fn read_property_observation(
                &self,
                credential: &$credential,
                did: &str,
                binding: PropertyBinding,
            ) -> Result<PropertyReading, CloudError> {
                PropertyApi::read_property_observation(self, credential, did, binding).await
            }
            pub async fn read_properties(
                &self,
                credential: &$credential,
                did: &str,
                bindings: &[PropertyBinding],
            ) -> Result<PropertyResults<PropertyReading, CloudError>, CloudError> {
                PropertyApi::read_properties(self, credential, did, bindings).await
            }
            pub async fn set_property(
                &self,
                credential: &$credential,
                did: &str,
                binding: PropertyBinding,
                value: bool,
            ) -> Result<(), CloudError> {
                PropertyApi::set_property(self, credential, did, binding, value).await
            }
            pub async fn set_properties(
                &self,
                credential: &$credential,
                did: &str,
                properties: &[(PropertyBinding, Value)],
            ) -> Result<PropertyResults<(), CloudError>, CloudError> {
                PropertyApi::set_properties(self, credential, did, properties).await
            }
        }
    };
}

impl_device_api!(CloudClient, str);
impl_device_api!(
    crate::xiaomi::mijia::MijiaClient,
    crate::xiaomi::mijia::MijiaSession
);
