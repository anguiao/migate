use super::App;
use futures_lite::future;
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

pub(super) const INITIAL_READ_LIMIT: Duration = Duration::from_secs(20);

/// A cold-start deadline shared by every Matter runner created during startup.
pub(super) struct StartupGate {
    deadline: Instant,
    complete: Cell<bool>,
}

impl StartupGate {
    pub(super) fn new(limit: Duration) -> Self {
        Self {
            deadline: Instant::now() + limit,
            complete: Cell::new(false),
        }
    }

    pub(super) fn prefer_runner(&self) -> bool {
        self.complete.get() || Instant::now() >= self.deadline
    }

    pub(super) async fn wait(&self, app: &App) {
        if self.complete.get() {
            return;
        }
        let reads_settled = async {
            loop {
                // Listen before checking the state so no change can be missed.
                let reads_changed = app.source.listen_initial_reads();
                let configuration_changed = app.committed.listen();
                if app.source.initial_reads_ready(&app.current().configuration) {
                    return true;
                }
                future::or(reads_changed, configuration_changed).await;
            }
        };
        let settled = future::or(reads_settled, async {
            async_io::Timer::at(self.deadline).await;
            false
        })
        .await;
        log::info!(
            "Initial device reads {}; starting Matter",
            if settled {
                "settled"
            } else {
                "are still pending"
            }
        );
        self.complete.set(true);
    }
}

#[cfg(test)]
#[path = "tests/startup.rs"]
mod tests;
