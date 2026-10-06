use super::model::AdapterError;
use event_listener::Event;
use futures_lite::future;
use futures_util::{TryStreamExt, stream::FuturesUnordered};
use std::{cell::RefCell, future::Future, pin::Pin};

type Task = Pin<Box<dyn Future<Output = Result<(), AdapterError>>>>;

/// Dropping the owner of `run` cancels every task in that lifecycle scope.
#[derive(Default)]
pub(super) struct TaskSet {
    incoming: RefCell<Vec<Task>>,
    changed: Event,
}
impl TaskSet {
    pub(super) fn spawn(&self, task: impl Future<Output = Result<(), AdapterError>> + 'static) {
        self.incoming.borrow_mut().push(Box::pin(task));
        self.changed.notify(usize::MAX);
    }
    pub(super) fn clear(&self) {
        self.incoming.borrow_mut().clear();
    }
    pub(super) async fn run(&self) -> Result<(), AdapterError> {
        let mut tasks = FuturesUnordered::<Task>::new();
        loop {
            let listener = self.changed.listen();
            tasks.extend(self.incoming.borrow_mut().drain(..));
            if tasks.is_empty() {
                listener.await;
                continue;
            }
            future::or(async { tasks.try_next().await.map(|_| ()) }, async {
                listener.await;
                Ok(())
            })
            .await?;
        }
    }
}
