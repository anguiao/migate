use super::{NETWORK_REFRESH_INTERVAL, ROUTE_TIMEOUT};
use crate::xiaomi::discovery::{
    DiscoveryBrowser, DiscoveryError, DiscoveryRegistry, GatewayCandidate, MdnsEvent, NetworkEpoch,
    NetworkMonitor, NetworkUpdate,
};
use futures_lite::future;
use futures_util::{FutureExt, future::LocalBoxFuture};
use std::time::{Duration, Instant};

type NetworkTaskResult = (NetworkMonitor, Result<NetworkUpdate, DiscoveryError>);
type BrowserResult = (DiscoveryBrowser, Result<MdnsEvent, DiscoveryError>);

pub(super) enum DiscoveryEvent {
    Network(NetworkTaskResult),
    Mdns(BrowserResult),
}

/// Owns interface discovery, mDNS browsing, and both refresh clocks.
pub(super) struct NetworkDiscovery {
    monitor: Option<NetworkMonitor>,
    task: Option<LocalBoxFuture<'static, NetworkTaskResult>>,
    network: Option<NetworkUpdate>,
    registry: Option<DiscoveryRegistry>,
    browser: Option<LocalBoxFuture<'static, BrowserResult>>,
    next_browser: Instant,
    next_network: Instant,
    epoch: u64,
}

impl NetworkDiscovery {
    pub(super) fn new() -> Self {
        Self {
            monitor: Some(NetworkMonitor::new()),
            task: None,
            network: None,
            registry: None,
            browser: None,
            next_browser: Instant::now(),
            next_network: Instant::now(),
            epoch: 0,
        }
    }

    pub(super) fn network(&self) -> Option<&NetworkUpdate> {
        self.network.as_ref()
    }
    pub(super) fn candidates(&self) -> Vec<GatewayCandidate> {
        self.registry
            .as_ref()
            .map(DiscoveryRegistry::candidates)
            .unwrap_or_default()
    }

    pub(super) fn schedule(&mut self, now: Instant, requested: bool) -> Result<(), DiscoveryError> {
        // A browser failure must not delay the independent interface refresh.
        let browser =
            if self.browser.is_none() && now >= self.next_browser && self.network.is_some() {
                self.start_browser()
            } else {
                Ok(())
            };
        if self.task.is_none()
            && (now >= self.next_network || requested)
            && let Some(mut monitor) = self.monitor.take()
        {
            self.task = Some(
                async move {
                    let result = monitor.refresh(ROUTE_TIMEOUT).await;
                    (monitor, result)
                }
                .boxed_local(),
            );
            self.next_network = now + NETWORK_REFRESH_INTERVAL;
        }
        browser
    }

    fn start_browser(&mut self) -> Result<(), DiscoveryError> {
        let network = self
            .network
            .as_ref()
            .expect("browser requires a network snapshot");
        match DiscoveryBrowser::start(&network.snapshot) {
            Ok(browser) => {
                self.browser = Some(browser_read(browser));
                Ok(())
            }
            Err(error) => {
                self.browser = None;
                self.next_browser = Instant::now() + Duration::from_secs(1);
                Err(error)
            }
        }
    }

    pub(super) fn refresh_scheduled(&self) -> bool {
        self.monitor.is_none() || self.task.is_some()
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        let network = (self.task.is_none() && self.monitor.is_some()).then_some(self.next_network);
        let browser =
            (self.browser.is_none() && self.network.is_some()).then_some(self.next_browser);
        network.into_iter().chain(browser).min()
    }

    pub(super) async fn next_event(&mut self) -> DiscoveryEvent {
        let network = async {
            match self.task.as_mut() {
                Some(task) => task.await,
                None => std::future::pending().await,
            }
        };
        let browser = async {
            match self.browser.as_mut() {
                Some(task) => task.await,
                None => std::future::pending().await,
            }
        };
        future::or(
            network.map(DiscoveryEvent::Network),
            browser.map(DiscoveryEvent::Mdns),
        )
        .await
    }

    pub(super) fn finish_network(
        &mut self,
        (monitor, result): NetworkTaskResult,
    ) -> Result<NetworkUpdate, DiscoveryError> {
        self.task = None;
        self.monitor = Some(monitor);
        self.next_network = Instant::now() + NETWORK_REFRESH_INTERVAL;
        result
    }

    // The runtime revokes old session authorities before installing a new network.
    pub(super) fn install(&mut self, update: NetworkUpdate) -> Result<(), DiscoveryError> {
        self.epoch = update.epoch.get();
        self.registry = Some(DiscoveryRegistry::new(
            update.epoch,
            update.snapshot.clone(),
        ));
        self.network = Some(update);
        self.start_browser()
    }

    pub(super) fn next_epoch(&self) -> NetworkEpoch {
        NetworkEpoch::new(self.epoch.saturating_add(1))
    }

    pub(super) fn clear(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        self.registry = None;
        self.browser = None;
        self.network = None;
    }

    pub(super) fn finish_browser(
        &mut self,
        (browser, event): BrowserResult,
    ) -> Result<bool, DiscoveryError> {
        match event {
            Ok(event) => {
                self.browser = Some(browser_read(browser));
                Ok(self
                    .registry
                    .as_mut()
                    .is_some_and(|registry| registry.apply(event).is_ok()))
            }
            Err(error) => {
                self.browser = None;
                self.next_browser = Instant::now() + Duration::from_secs(1);
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(super) fn set_network(&mut self, update: NetworkUpdate) {
        self.network = Some(update);
    }
    #[cfg(test)]
    pub(super) fn set_registry(&mut self, registry: DiscoveryRegistry) {
        self.registry = Some(registry);
    }
    #[cfg(test)]
    pub(super) fn set_epoch(&mut self, epoch: u64) {
        self.epoch = epoch;
    }
    #[cfg(test)]
    pub(super) fn disable_monitor(&mut self) {
        self.monitor = None;
    }
    #[cfg(test)]
    pub(super) fn defer_browser_until(&mut self, next: Instant) {
        self.next_browser = next;
    }
    #[cfg(test)]
    pub(super) fn defer_network_until(&mut self, next: Instant) {
        self.next_network = next;
    }
    #[cfg(test)]
    pub(super) fn cancel(&mut self) {
        self.task = None;
        self.browser = None;
    }
}

fn browser_read(browser: DiscoveryBrowser) -> LocalBoxFuture<'static, BrowserResult> {
    async move {
        let result = browser.next_event().await;
        (browser, result)
    }
    .boxed_local()
}
