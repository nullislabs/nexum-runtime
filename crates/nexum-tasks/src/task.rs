//! Typed handles over spawned tasks and the abort-on-drop set the event
//! loop drains at shutdown.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use tracing::debug;

/// A source task's report that its source cannot continue and must not be
/// reopened.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SourceTermination {
    /// Owning module; `None` when the source is shared across modules.
    pub module: Option<String>,
    /// Chain the source was ingesting.
    pub chain_id: u64,
    /// Operator-facing reason.
    pub reason: String,
}

/// Why a pump task returned.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TaskExit {
    /// The receiver the task feeds was dropped, so it stopped pumping; the
    /// ordinary exit at shutdown.
    ReceiverGone,
    /// The task's source is unrecoverable.
    SourceTerminal(SourceTermination),
}

/// Handle to one spawned task.
#[derive(Debug)]
pub struct TaskHandle<T>(pub(crate) tokio::task::JoinHandle<T>);

impl<T> TaskHandle<T> {
    /// Request cancellation; the task stops at its next await point.
    pub fn abort(&self) {
        self.0.abort();
    }

    /// Wait for the task to finish. `None` when it was aborted or panicked.
    pub async fn join(self) -> Option<T> {
        self.0.await.ok()
    }
}

/// The pump-task handles the event loop owns for its lifetime; abortable as
/// a set so every task is observed to finish before the engine returns.
#[derive(Debug, Default)]
pub struct TaskSet {
    handles: Vec<TaskHandle<TaskExit>>,
}

impl TaskSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take ownership of a freshly spawned task's handle.
    pub fn push(&mut self, handle: TaskHandle<TaskExit>) {
        self.handles.push(handle);
    }

    /// Resolve with the exit of the next task to finish on its own, removing
    /// its handle; pends while none has, including on an empty set. A
    /// panicked or aborted task is discarded rather than yielded.
    /// Cancel-safe.
    pub async fn join_next(&mut self) -> TaskExit {
        std::future::poll_fn(|cx| {
            let mut i = 0;
            while i < self.handles.len() {
                match Pin::new(&mut self.handles[i].0).poll(cx) {
                    Poll::Ready(Ok(exit)) => {
                        self.handles.swap_remove(i);
                        return Poll::Ready(exit);
                    }
                    // The swapped-in handle re-polls at the same index.
                    Poll::Ready(Err(_)) => {
                        self.handles.swap_remove(i);
                    }
                    Poll::Pending => i += 1,
                }
            }
            Poll::Pending
        })
        .await
    }

    /// Abort every task, then await each handle so all tasks are observed
    /// to finish. A `None` join (aborted or panicked) counts against the
    /// aborted tally in the drain summary.
    pub async fn shutdown(mut self) {
        for handle in &self.handles {
            handle.abort();
        }
        let total = self.handles.len();
        let mut clean = 0usize;
        let mut aborted = 0usize;
        for handle in self.handles.drain(..) {
            match handle.join().await {
                Some(_) => clean += 1,
                None => aborted += 1,
            }
        }
        debug!(total, clean, aborted, "pump task set drained");
    }
}

impl Drop for TaskSet {
    /// Abort any handles [`shutdown`](TaskSet::shutdown) did not drain, so
    /// the tasks do not detach and outlive the engine (a bare `JoinHandle`
    /// detaches on drop).
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::TaskManager;

    fn termination() -> SourceTermination {
        SourceTermination {
            module: Some("mod".to_owned()),
            chain_id: 1,
            reason: "unrecoverable".to_owned(),
        }
    }

    #[tokio::test]
    async fn join_next_yields_a_finished_task_and_removes_its_handle() {
        let manager = TaskManager::new();
        let mut set = TaskSet::new();
        set.push(
            manager
                .executor()
                .spawn(async { TaskExit::SourceTerminal(termination()) }),
        );
        set.push(manager.executor().spawn(async {
            std::future::pending::<()>().await;
            TaskExit::ReceiverGone
        }));

        let exit = tokio::time::timeout(Duration::from_secs(5), set.join_next())
            .await
            .expect("a finished task resolves join_next");
        assert_eq!(exit, TaskExit::SourceTerminal(termination()));

        let idle = tokio::time::timeout(Duration::from_millis(50), set.join_next()).await;
        assert!(
            idle.is_err(),
            "the still-running task keeps join_next pending"
        );
        set.shutdown().await;
    }

    #[tokio::test]
    async fn join_next_pends_on_an_empty_set() {
        let mut set = TaskSet::new();
        let idle = tokio::time::timeout(Duration::from_millis(50), set.join_next()).await;
        assert!(idle.is_err(), "an empty set never resolves join_next");
    }

    #[tokio::test]
    async fn join_next_discards_a_panicked_task_without_a_yield() {
        let manager = TaskManager::new();
        let mut set = TaskSet::new();
        set.push(manager.executor().spawn(async { panic!("boom") }));
        let idle = tokio::time::timeout(Duration::from_millis(50), set.join_next()).await;
        assert!(idle.is_err(), "a panic is not an observable exit");
        set.shutdown().await;
    }
}
