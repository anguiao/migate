use std::{
    future::Future,
    pin::Pin,
    time::{Duration, Instant},
};
pub(in crate::matter::lighting) trait Clock {
    fn now(&self) -> u64;
    fn wait_until(&self, deadline: u64) -> Pin<Box<dyn Future<Output = ()> + '_>>;
}
pub(in crate::matter::lighting) struct SystemClock(Instant);
impl SystemClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}
impl Clock for SystemClock {
    fn now(&self) -> u64 {
        self.0.elapsed().as_millis() as u64
    }
    fn wait_until(&self, deadline: u64) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async move {
            async_io::Timer::after(Duration::from_millis(deadline.saturating_sub(self.now())))
                .await;
        })
    }
}
