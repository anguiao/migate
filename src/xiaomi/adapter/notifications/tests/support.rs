use super::super::*;

#[derive(Clone, Copy)]
pub(in crate::xiaomi::adapter) enum NotificationConnector {
    Disabled,
    Plaintext(std::net::SocketAddr),
}

pub(in crate::xiaomi::adapter::notifications) async fn connect(
    context: &Context,
    credentials: &XiaomiRecord,
) -> Result<(NotificationHandle, NotificationDriver), NotificationError> {
    match context.notification_connector.get() {
        NotificationConnector::Disabled => future::pending().await,
        NotificationConnector::Plaintext(address) => {
            crate::xiaomi::cloud::notification_test_support::connect(address, credentials).await
        }
    }
}
