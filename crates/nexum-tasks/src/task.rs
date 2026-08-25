//! Typed handles over spawned tasks and the abort-on-drop set the event
//! loop drains at shutdown.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
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
    handles: Vec<(Arc<str>, TaskHandle<TaskExit>)>,
    died: Vec<Arc<str>>,
}

impl TaskSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take ownership of a freshly spawned task's handle under `label`, the
    /// name every report of that task carries.
    pub fn push(&mut self, label: impl Into<Arc<str>>, handle: TaskHandle<TaskExit>) {
        self.handles.push((label.into(), handle));
    }

    /// Labels of the tasks that ended without an exit, in the order the set
    /// observed them; see [`join_next`](TaskSet::join_next).
    pub fn died(&self) -> &[Arc<str>] {
        &self.died
    }

    /// Resolve with the exit of the next task to finish on its own, removing
    /// its handle; pends while none has, including on an empty set. A
    /// panicked or aborted task is discarded rather than yielded, its label
    /// recorded in [`died`](TaskSet::died). Cancel-safe.
    ///
    /// Only `cargo test` forces the unwind a panic needs to reach that
    /// discard; `panic = "abort"` takes the process down in every other
    /// build. An abort reaches it in any build.
    pub async fn join_next(&mut self) -> TaskExit {
        std::future::poll_fn(|cx| {
            let mut i = 0;
            while i < self.handles.len() {
                let (_, handle) = &mut self.handles[i];
                match Pin::new(&mut handle.0).poll(cx) {
                    Poll::Ready(Ok(exit)) => {
                        self.handles.swap_remove(i);
                        return Poll::Ready(exit);
                    }
                    // The swapped-in handle re-polls at the same index.
                    Poll::Ready(Err(_)) => {
                        let (label, _) = self.handles.swap_remove(i);
                        self.died.push(label);
                    }
                    Poll::Pending => i += 1,
                }
            }
            Poll::Pending
        })
        .await
    }

    /// Abort every task, then await each handle so all tasks are observed
    /// to finish. A `None` join (aborted or panicked) is named in the drain
    /// summary's aborted list.
    pub async fn shutdown(mut self) {
        for (_, handle) in &self.handles {
            handle.abort();
        }
        let total = self.handles.len();
        let mut clean = 0usize;
        let mut aborted: Vec<Arc<str>> = Vec::new();
        for (label, handle) in self.handles.drain(..) {
            match handle.join().await {
                Some(_) => clean += 1,
                None => aborted.push(label),
            }
        }
        debug!(
            total,
            clean,
            aborted = aborted.len(),
            aborted_tasks = %aborted.join(", "),
            "pump task set drained"
        );
    }
}

impl Drop for TaskSet {
    /// Abort any handles [`shutdown`](TaskSet::shutdown) did not drain, so
    /// the tasks do not detach and outlive the engine (a bare `JoinHandle`
    /// detaches on drop).
    fn drop(&mut self) {
        for (_, handle) in &self.handles {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tracing::instrument::WithSubscriber;

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
            "terminal",
            manager
                .executor()
                .spawn(async { TaskExit::SourceTerminal(termination()) }),
        );
        set.push(
            "live",
            manager.executor().spawn(async {
                std::future::pending::<()>().await;
                TaskExit::ReceiverGone
            }),
        );

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
        set.push(
            "panicking",
            manager.executor().spawn(async { panic!("boom") }),
        );
        let idle = tokio::time::timeout(Duration::from_millis(50), set.join_next()).await;
        assert!(idle.is_err(), "a panic is not an observable exit");
        set.shutdown().await;
    }

    #[tokio::test]
    async fn join_next_records_the_label_of_a_task_it_discards() {
        let manager = TaskManager::new();
        let mut set = TaskSet::new();
        let handle = manager.executor().spawn(async {
            std::future::pending::<()>().await;
            TaskExit::ReceiverGone
        });
        handle.abort();
        set.push("chain-log:1:mod", handle);
        set.push(
            "block:1",
            manager.executor().spawn(async {
                std::future::pending::<()>().await;
                TaskExit::ReceiverGone
            }),
        );

        let idle = tokio::time::timeout(Duration::from_millis(50), set.join_next()).await;
        assert!(idle.is_err(), "an aborted task is not an observable exit");
        assert_eq!(
            set.died().iter().map(|l| &**l).collect::<Vec<_>>(),
            vec!["chain-log:1:mod"],
            "the dead pump is the only one named",
        );
        set.shutdown().await;
    }

    #[tokio::test]
    async fn the_drain_summary_names_every_task_that_did_not_stop_cleanly() {
        let manager = TaskManager::new();
        let mut set = TaskSet::new();
        set.push(
            "block:1",
            manager.executor().spawn(async {
                std::future::pending::<()>().await;
                TaskExit::ReceiverGone
            }),
        );
        let (ran_tx, ran_rx) = tokio::sync::oneshot::channel();
        set.push(
            "chain-log:1:mod",
            manager.executor().spawn(async move {
                let _ = ran_tx.send(());
                TaskExit::ReceiverGone
            }),
        );
        ran_rx.await.expect("the clean task ran before the drain");

        let sink = Sink::default();
        let collector = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish();
        set.shutdown().with_subscriber(collector).await;

        let out = String::from_utf8(sink.0.lock().expect("sink is not poisoned").clone())
            .expect("log output is UTF-8");
        let summary = out
            .lines()
            .find(|line| line.contains("pump task set drained"))
            .expect("the drain logs a summary");
        assert!(
            summary.contains("block:1"),
            "the aborted pump is named: {summary}",
        );
        assert!(
            !summary.contains("chain-log:1:mod"),
            "a clean stop is not named as aborted: {summary}",
        );
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("sink is not poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;

        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }
}
