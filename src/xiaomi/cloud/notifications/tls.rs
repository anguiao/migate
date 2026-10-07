use super::NotificationError;
use async_io::Async;
use futures_lite::future;
use futures_rustls::{TlsConnector, client::TlsStream};
use rustls::{
    ClientConfig, client::danger::ServerCertVerifier, crypto::CryptoProvider, pki_types::ServerName,
};
use std::{
    future::Future,
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    sync::Arc,
    time::{Duration, Instant},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn connect(
    host: &str,
    port: u16,
) -> Result<TlsStream<Async<TcpStream>>, NotificationError> {
    let name =
        ServerName::try_from(host.to_owned()).map_err(|_| NotificationError::InvalidInput)?;
    if port == 0 {
        return Err(NotificationError::InvalidInput);
    }
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let host = host.to_owned();
    // Native trust evaluation and DNS resolution can block. Keep the caller's
    // deadline independent of those operations; no MQTT data is sent here.
    let connection = blocking::unblock(move || {
        future::block_on(async {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
                .map_err(|_| NotificationError::Tls)?;
            let config = client_config(provider, Arc::new(verifier))?;
            let addresses = (host.as_str(), port)
                .to_socket_addrs()
                .map_err(|_| NotificationError::Io)?
                .collect();
            with_deadline(connect_addresses(name, addresses, config), deadline).await
        })
    });
    with_deadline(connection, deadline).await
}

fn client_config(
    provider: Arc<CryptoProvider>,
    verifier: Arc<dyn ServerCertVerifier>,
) -> Result<Arc<ClientConfig>, NotificationError> {
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| NotificationError::Tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

async fn connect_addresses(
    name: ServerName<'static>,
    addresses: Vec<SocketAddr>,
    config: Arc<ClientConfig>,
) -> Result<TlsStream<Async<TcpStream>>, NotificationError> {
    for address in addresses {
        if let Ok(socket) = Async::<TcpStream>::connect(address).await {
            return TlsConnector::from(config)
                .connect(name, socket)
                .await
                .map_err(|_| NotificationError::Tls);
        }
    }
    Err(NotificationError::Io)
}

async fn with_deadline<T>(
    operation: impl Future<Output = Result<T, NotificationError>>,
    deadline: Instant,
) -> Result<T, NotificationError> {
    if Instant::now() >= deadline {
        return Err(NotificationError::Timeout);
    }
    future::race(operation, async {
        async_io::Timer::at(deadline).await;
        Err(NotificationError::Timeout)
    })
    .await
}

#[cfg(test)]
mod tests;
