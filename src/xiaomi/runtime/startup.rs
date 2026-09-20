use std::{
    fmt,
    net::SocketAddrV4,
    time::{Duration, Instant},
};

use async_io::Timer;
use flume::Receiver;
use futures_lite::future;
use futures_util::{FutureExt, TryFutureExt, future::LocalBoxFuture};

use crate::xiaomi::{
    cloud::{CloudNotification, CloudNotificationHandle, CloudNotificationSession},
    discovery::NetworkEpoch,
    gateway::{GatewayEvidence, GatewayHandle, GatewayNotification, GatewaySession},
    lan::{
        LanError, LanEvidence, LanHandle, LanNotification, LanProperty, LanSendGuard, LanSession,
        LanTarget,
    },
    mqtt::{
        CloudTlsConfig, GatewayTlsConfig, MqttConfig, MqttConnection, MqttError, MqttSendGuard,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LanOperationEvidence {
    Native,
    Mcn02Legacy,
    ReadOnly,
}

pub(crate) struct RunningLan {
    pub handle: LanHandle,
    pub notifications: Receiver<LanNotification>,
    pub evidence: LanEvidence,
    pub operation: LanOperationEvidence,
    task: LocalBoxFuture<'static, Result<(), LanError>>,
}

impl RunningLan {
    pub fn into_parts(
        self,
    ) -> (
        LanHandle,
        Receiver<LanNotification>,
        LanEvidence,
        LanOperationEvidence,
        LocalBoxFuture<'static, Result<(), LanError>>,
    ) {
        (
            self.handle,
            self.notifications,
            self.evidence,
            self.operation,
            self.task,
        )
    }
}

pub(crate) async fn start_lan(
    target: LanTarget,
    virtual_did: u64,
    authentication_property: LanProperty,
    deadline: Instant,
    guard: LanSendGuard,
) -> Result<RunningLan, LanError> {
    let (session, handle, notifications) = LanSession::new(target, virtual_did)?;
    start_lan_session(
        session,
        handle,
        notifications,
        authentication_property,
        deadline,
        guard,
    )
    .await
}

async fn start_lan_session(
    session: LanSession,
    handle: LanHandle,
    notifications: Receiver<LanNotification>,
    authentication_property: LanProperty,
    deadline: Instant,
    guard: LanSendGuard,
) -> Result<RunningLan, LanError> {
    let mut task = session.run().boxed_local();
    let authentication_handle = handle.clone();
    let authenticate =
        authentication_handle.authenticate(authentication_property, deadline, guard.child());
    futures_lite::pin!(authenticate);
    let evidence = match future::or(
        authenticate.map(Some),
        task.as_mut().map(|result| {
            let _ = result;
            None
        }),
    )
    .await
    {
        Some(result) => result?,
        None => return Err(LanError::session_stopped()),
    };
    let operation = if evidence.native_supported {
        LanOperationEvidence::Native
    } else if handle.model() == "lumi.acpartner.mcn02" {
        let legacy = handle
            .read_mcn02(deadline, guard.child())
            .map(|result| result.map(|_| LanOperationEvidence::Mcn02Legacy));
        futures_lite::pin!(legacy);
        match future::or(
            legacy.map(Some),
            task.as_mut().map(|result| {
                let _ = result;
                None
            }),
        )
        .await
        {
            Some(Ok(operation)) => operation,
            Some(Err(_)) => LanOperationEvidence::ReadOnly,
            None => return Err(LanError::session_stopped()),
        }
    } else {
        LanOperationEvidence::ReadOnly
    };
    Ok(RunningLan {
        handle,
        notifications,
        evidence,
        operation,
        task,
    })
}

#[cfg(test)]
pub(crate) async fn start_lan_session_for_test(
    session: LanSession,
    handle: LanHandle,
    notifications: Receiver<LanNotification>,
    authentication_property: LanProperty,
    deadline: Instant,
    guard: LanSendGuard,
) -> Result<RunningLan, LanError> {
    start_lan_session(
        session,
        handle,
        notifications,
        authentication_property,
        deadline,
        guard,
    )
    .await
}

#[derive(Debug)]
pub(crate) enum TransportStartupError {
    Timeout,
    Mqtt(MqttError),
    Gateway(crate::xiaomi::gateway::GatewayError),
}

impl fmt::Display for TransportStartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("transport startup timed out"),
            Self::Mqtt(error) => write!(formatter, "{error}"),
            Self::Gateway(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for TransportStartupError {}

pub(crate) struct GatewayStartup {
    pub endpoint: SocketAddrV4,
    pub tls: GatewayTlsConfig,
    pub mqtt: MqttConfig,
    pub virtual_did: String,
    pub gateway_did: u64,
    pub peer_did: String,
    pub epoch: NetworkEpoch,
    pub deadline: Instant,
    pub guard: MqttSendGuard,
}

pub(crate) struct RunningGateway {
    pub handle: GatewayHandle,
    pub notifications: Receiver<GatewayNotification>,
    pub evidence: GatewayEvidence,
    driver: LocalBoxFuture<'static, Result<(), MqttError>>,
    session: LocalBoxFuture<'static, Result<(), crate::xiaomi::gateway::GatewayError>>,
}

pub(crate) struct GatewayRuntimeParts {
    pub handle: GatewayHandle,
    pub notifications: Receiver<GatewayNotification>,
    pub evidence: GatewayEvidence,
    pub task: LocalBoxFuture<'static, Result<(), TransportStartupError>>,
}

impl RunningGateway {
    pub(crate) fn into_parts(self) -> GatewayRuntimeParts {
        GatewayRuntimeParts {
            handle: self.handle,
            notifications: self.notifications,
            evidence: self.evidence,
            task: future::or(
                self.driver.map_err(TransportStartupError::Mqtt),
                self.session.map_err(TransportStartupError::Gateway),
            )
            .boxed_local(),
        }
    }
}

pub(crate) async fn start_gateway(
    startup: GatewayStartup,
) -> Result<RunningGateway, TransportStartupError> {
    let remaining = startup.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(TransportStartupError::Timeout);
    }
    let mqtt_config = startup
        .mqtt
        .with_endpoint(startup.endpoint.ip().to_string(), startup.endpoint.port())
        .with_tls(startup.tls.client_config());
    let (mqtt, mqtt_handle, messages) =
        MqttConnection::new(mqtt_config).map_err(TransportStartupError::Mqtt)?;
    let (gateway, handle, notifications) = GatewaySession::new(
        &startup.virtual_did,
        startup.gateway_did,
        &startup.peer_did,
        startup.epoch,
        mqtt_handle,
        messages,
    )
    .map_err(TransportStartupError::Gateway)?;
    let mut driver = mqtt.run().boxed_local();
    let mut session = gateway
        .run_guarded(startup.deadline, startup.guard.clone())
        .boxed_local();
    enum Ready {
        Evidence(Result<GatewayEvidence, crate::xiaomi::gateway::GatewayError>),
        Driver(Result<(), MqttError>),
        Session(Result<(), crate::xiaomi::gateway::GatewayError>),
        Timeout,
    }
    let ready = {
        let startup_handle = handle.clone();
        let evidence = async move {
            startup_handle
                .get_devices(startup.deadline, startup.guard)
                .await
        };
        futures_lite::pin!(evidence);
        future::or(
            evidence.map(Ready::Evidence),
            future::or(
                driver.as_mut().map(Ready::Driver),
                future::or(session.as_mut().map(Ready::Session), async {
                    Timer::at(startup.deadline).await;
                    Ready::Timeout
                }),
            ),
        )
        .await
    };
    let evidence = match ready {
        Ready::Evidence(result) => result.map_err(TransportStartupError::Gateway)?,
        Ready::Driver(result) => {
            return Err(result.map_or_else(TransportStartupError::Mqtt, |_| {
                TransportStartupError::Timeout
            }));
        }
        Ready::Session(result) => {
            return Err(result.map_or_else(TransportStartupError::Gateway, |_| {
                TransportStartupError::Timeout
            }));
        }
        Ready::Timeout => return Err(TransportStartupError::Timeout),
    };
    Ok(RunningGateway {
        handle,
        notifications,
        evidence,
        driver,
        session,
    })
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_gateway_plain_for_test(
    endpoint: SocketAddrV4,
    mqtt_config: MqttConfig,
    virtual_did: &str,
    gateway_did: u64,
    peer_did: &str,
    epoch: NetworkEpoch,
    guard: MqttSendGuard,
    deadline: Instant,
) -> Result<RunningGateway, TransportStartupError> {
    let (mqtt, mqtt_handle, messages) =
        MqttConnection::new(mqtt_config.with_endpoint(endpoint.ip().to_string(), endpoint.port()))
            .map_err(TransportStartupError::Mqtt)?;
    let (gateway, handle, notifications) = GatewaySession::new(
        virtual_did,
        gateway_did,
        peer_did,
        epoch,
        mqtt_handle,
        messages,
    )
    .map_err(TransportStartupError::Gateway)?;
    let mut driver = mqtt.run().boxed_local();
    let mut session = gateway.run_guarded(deadline, guard.clone()).boxed_local();
    enum Ready {
        Evidence(Result<GatewayEvidence, crate::xiaomi::gateway::GatewayError>),
        Driver(Result<(), MqttError>),
        Session(Result<(), crate::xiaomi::gateway::GatewayError>),
        Timeout,
    }
    let request_handle = handle.clone();
    let request = request_handle.get_devices(deadline, guard);
    futures_lite::pin!(request);
    let ready = future::or(
        request.map(Ready::Evidence),
        future::or(
            driver.as_mut().map(Ready::Driver),
            future::or(session.as_mut().map(Ready::Session), async {
                Timer::at(deadline).await;
                Ready::Timeout
            }),
        ),
    )
    .await;
    let evidence = match ready {
        Ready::Evidence(result) => result.map_err(TransportStartupError::Gateway)?,
        Ready::Driver(result) => {
            return Err(result.map_or_else(TransportStartupError::Mqtt, |_| {
                TransportStartupError::Timeout
            }));
        }
        Ready::Session(result) => {
            return Err(result.map_or_else(TransportStartupError::Gateway, |_| {
                TransportStartupError::Timeout
            }));
        }
        Ready::Timeout => return Err(TransportStartupError::Timeout),
    };
    Ok(RunningGateway {
        handle,
        notifications,
        evidence,
        driver,
        session,
    })
}

pub(crate) struct CloudNotificationStartup {
    pub host: String,
    pub port: u16,
    pub tls: CloudTlsConfig,
    pub oauth_client_uuid: String,
    pub access_token: String,
    pub keep_alive: Duration,
    pub dids: Vec<String>,
    pub deadline: Instant,
}

pub(crate) struct RunningCloudNotifications {
    pub handle: CloudNotificationHandle,
    pub notifications: Receiver<CloudNotification>,
    pub generation: u64,
    driver: LocalBoxFuture<'static, Result<(), MqttError>>,
    session: LocalBoxFuture<'static, Result<(), MqttError>>,
}

impl RunningCloudNotifications {
    pub fn into_parts(
        self,
    ) -> (
        CloudNotificationHandle,
        Receiver<CloudNotification>,
        u64,
        LocalBoxFuture<'static, Result<(), TransportStartupError>>,
    ) {
        let task = future::or(
            self.driver.map_err(TransportStartupError::Mqtt),
            self.session.map_err(TransportStartupError::Mqtt),
        )
        .boxed_local();
        (self.handle, self.notifications, self.generation, task)
    }
}

pub(crate) async fn start_cloud_notifications(
    startup: CloudNotificationStartup,
) -> Result<RunningCloudNotifications, TransportStartupError> {
    let (mqtt, cloud, handle, notifications) = CloudNotificationSession::new(
        &startup.oauth_client_uuid,
        &startup.access_token,
        startup.keep_alive,
        &startup.host,
        startup.port,
        Some(
            startup
                .tls
                .client_config_for(&startup.host)
                .map_err(TransportStartupError::Mqtt)?,
        ),
    )
    .map_err(TransportStartupError::Mqtt)?;
    let mut driver = mqtt.run().boxed_local();
    let mut session = cloud.run().boxed_local();
    enum Ready {
        Selected(Result<u64, MqttError>),
        Driver(Result<(), MqttError>),
        Session(Result<(), MqttError>),
        Timeout,
    }
    let ready = {
        let startup_handle = handle.clone();
        let selected = async move {
            startup_handle
                .select_dids(startup.dids, startup.deadline)
                .await
        };
        futures_lite::pin!(selected);
        future::or(
            selected.map(Ready::Selected),
            future::or(
                driver.as_mut().map(Ready::Driver),
                future::or(session.as_mut().map(Ready::Session), async {
                    Timer::at(startup.deadline).await;
                    Ready::Timeout
                }),
            ),
        )
        .await
    };
    let generation = match ready {
        Ready::Selected(result) => result.map_err(TransportStartupError::Mqtt)?,
        Ready::Driver(result) | Ready::Session(result) => {
            return Err(result.map_or_else(TransportStartupError::Mqtt, |_| {
                TransportStartupError::Timeout
            }));
        }
        Ready::Timeout => return Err(TransportStartupError::Timeout),
    };
    Ok(RunningCloudNotifications {
        handle,
        notifications,
        generation,
        driver,
        session,
    })
}
