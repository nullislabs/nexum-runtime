//! The readiness surface: per-module lifecycle state, published by the event
//! loop and sampled by an operator probe.

use strum::VariantArray as _;
use tokio::sync::watch;

use super::lifecycle::ModuleState;
use nexum_primitives::module_id::ModuleId;

/// Every loaded module and its state, in `[[modules]]` declaration order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HealthSnapshot {
    modules: Vec<(ModuleId, ModuleState)>,
}

impl HealthSnapshot {
    /// One dispatchable module is enough: a single quarantined module must
    /// not pull an engine still serving every other module out of rotation.
    pub fn ready(&self) -> bool {
        self.modules
            .iter()
            .any(|(_, state)| *state == ModuleState::Alive)
    }

    /// The detail the aggregate flattens.
    pub fn modules(&self) -> impl ExactSizeIterator<Item = (&ModuleId, ModuleState)> {
        self.modules.iter().map(|(name, state)| (name, *state))
    }
}

/// The supervisor's write side.
pub struct HealthPublisher(watch::Sender<HealthSnapshot>);

/// A probe's read side. Sampling reads the last published snapshot and never
/// waits on the event loop.
#[derive(Clone, Debug)]
pub struct HealthWatch(watch::Receiver<HealthSnapshot>);

/// Both ends of the readiness channel.
///
/// The snapshot starts empty, so a probe reads not-ready from process start
/// until the supervisor has booted and published.
pub fn health_channel() -> (HealthPublisher, HealthWatch) {
    let (tx, rx) = watch::channel(HealthSnapshot::default());
    (HealthPublisher(tx), HealthWatch(rx))
}

impl HealthWatch {
    /// The last published snapshot.
    pub fn snapshot(&self) -> HealthSnapshot {
        self.0.borrow().clone()
    }
}

impl HealthPublisher {
    /// Publish `states`, rewriting the per-state gauge only when something
    /// moved, so a quiet engine records nothing per trigger.
    pub fn publish(&self, states: impl IntoIterator<Item = (ModuleId, ModuleState)>) {
        let next = HealthSnapshot {
            modules: states.into_iter().collect(),
        };
        self.0.send_if_modified(|current| {
            if *current == next {
                return false;
            }
            *current = next;
            report_states(current);
            true
        });
    }
}

/// Four series per module, `1` on its state and `0` on the other three: a
/// transition must clear the state it left rather than leave two series at `1`.
fn report_states(snapshot: &HealthSnapshot) {
    for (name, state) in snapshot.modules() {
        for candidate in ModuleState::VARIANTS {
            metrics::gauge!(
                "nexum_runtime_module_state",
                "module" => name.clone(),
                "state" => <&str>::from(candidate),
            )
            .set(if *candidate == state { 1.0 } else { 0.0 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str) -> ModuleId {
        ModuleId::parse(name).expect("a valid module name")
    }

    fn snapshot(states: &[(&str, ModuleState)]) -> HealthSnapshot {
        let (publisher, watch) = health_channel();
        publisher.publish(states.iter().map(|(name, state)| (module(name), *state)));
        watch.snapshot()
    }

    /// The gauge labels are an operator contract; a renamed variant renames
    /// a series.
    #[test]
    fn each_state_carries_its_snake_case_label() {
        let labels: Vec<&str> = ModuleState::VARIANTS.iter().map(<&str>::from).collect();
        assert_eq!(labels, ["alive", "backoff", "dead", "poisoned"]);
    }

    #[test]
    fn an_unpublished_snapshot_is_not_ready() {
        let (_publisher, watch) = health_channel();
        let snapshot = watch.snapshot();
        assert!(!snapshot.ready());
        assert_eq!(snapshot.modules().len(), 0);
    }

    #[test]
    fn one_alive_module_beside_a_poisoned_one_is_ready() {
        assert!(
            snapshot(&[
                ("quarantined", ModuleState::Poisoned),
                ("serving", ModuleState::Alive),
            ])
            .ready()
        );
    }

    #[test]
    fn backoff_and_dead_are_not_dispatchable_so_the_process_is_not_ready() {
        assert!(
            !snapshot(&[
                ("waiting", ModuleState::Backoff),
                ("gone", ModuleState::Dead)
            ])
            .ready()
        );
    }

    #[test]
    fn the_snapshot_keeps_declaration_order() {
        let snapshot = snapshot(&[("zulu", ModuleState::Alive), ("alpha", ModuleState::Dead)]);
        let names: Vec<&str> = snapshot.modules().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["zulu", "alpha"]);
    }

    #[test]
    fn publishing_the_same_states_twice_does_not_rewrite() {
        let (publisher, watch) = health_channel();
        let states = || std::iter::once((module("only"), ModuleState::Alive));
        publisher.publish(states());
        let mut rx = watch.0.clone();
        rx.mark_unchanged();
        publisher.publish(states());
        assert!(!rx.has_changed().expect("the sender is alive"));
        publisher.publish(std::iter::once((module("only"), ModuleState::Backoff)));
        assert!(rx.has_changed().expect("the sender is alive"));
    }
}
