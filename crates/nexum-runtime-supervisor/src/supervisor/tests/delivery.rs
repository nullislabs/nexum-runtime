//! Delivery measures: the host wait from source receipt to handler start,
//! and the block age at that instant.

use std::time::UNIX_EPOCH;

use futures::StreamExt as _;
use nexum_primitives::module_id::ModuleId;
use nexum_runtime_api::ExtensionDelivery;
use nexum_tasks::{TaskManager, TaskSet};
use nexum_world::ChainMethod;

use super::*;
use crate::engine_config::DispatchLimitsSection;
use crate::runtime::event_loop::{
    ChainLogItem, TaggedBlockStream, TaggedChainLogStream, open_block_streams,
    open_chain_log_streams, run,
};
use crate::supervisor::{EventSource, ReceivedAt};
use crate::test_utils::metrics_util::debugging::DebugValue;
use crate::test_utils::{
    FakeNode, ManualClock, MockStateStore, Sample, capture_metrics, mock_components_from,
    samples_named,
};

const WAIT: &str = "nexum_runtime_delivery_latency_seconds";
const AGE: &str = "nexum_runtime_dispatch_block_age_seconds";
const HANDLER: &str = "nexum_runtime_dispatch_latency_seconds";

/// The block timestamp every test dispatches, in seconds.
const BLOCK_TIME: u64 = 1_700_000_000;

/// The wait every stamped test puts between the receipt and the dispatch.
const QUEUED: Duration = Duration::from_millis(250);

/// How long the slow-host fixture's first handler holds the fan-out.
const PARKED: Duration = Duration::from_secs(2);

fn block_at(timestamp_secs: u64) -> nexum::host::types::Block {
    nexum::host::types::Block {
        chain_id: 1,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: timestamp_secs * 1000,
    }
}

fn module_id(name: &str) -> ModuleId {
    ModuleId::parse(name).expect("a valid module name")
}

/// A wall clock pinned `offset_secs` away from [`BLOCK_TIME`].
fn wall_clock_at(offset_secs: i64) -> ManualClock {
    let clock = ManualClock::new();
    clock.set(UNIX_EPOCH + Duration::from_secs(BLOCK_TIME.saturating_add_signed(offset_secs)));
    clock
}

/// Every observation `name` recorded under the given labels, in order.
fn observed(samples: &[Sample], name: &str, labels: &[(&str, &str)]) -> Vec<f64> {
    samples_named(samples, name)
        .into_iter()
        .filter(|sample| {
            labels
                .iter()
                .all(|(key, value)| sample.has_label(key, value))
        })
        .flat_map(|sample| match &sample.value {
            DebugValue::Histogram(values) => values.iter().map(|v| v.into_inner()),
            other => panic!("{name} is not a histogram: {other:?}"),
        })
        .collect()
}

/// A sum of two recorded durations, each already rounded to an `f64`.
fn assert_adds_up(total: f64, parts: [f64; 2], what: &str) {
    let sum = parts[0] + parts[1];
    assert!(
        (total - sum).abs() < 1e-9,
        "{what}: {total} is not {} + {}",
        parts[0],
        parts[1],
    );
}

/// The label keys of the one series `name` recorded.
fn label_keys<'a>(samples: &'a [Sample], name: &str) -> Vec<&'a str> {
    let hits = samples_named(samples, name);
    assert_eq!(hits.len(), 1, "one {name} series: {samples:?}");
    hits[0].labels.iter().map(|(key, _)| key.as_str()).collect()
}

/// A stamp, then [`QUEUED`] of paused time, so the wait a dispatch records is
/// exact.
async fn received_a_while_ago() -> ReceivedAt {
    tokio::time::pause();
    let received = ReceivedAt::now();
    tokio::time::advance(QUEUED).await;
    received
}

/// Declares the `ticks` kind and opens no source for it.
struct Ticks;

impl Extension<LocalTypes> for Ticks {
    fn namespace(&self) -> &'static str {
        "ticks"
    }
    fn capabilities(&self) -> manifest::NamespaceCaps {
        manifest::NamespaceCaps {
            prefix: "test:ticks/",
            ifaces: &[],
        }
    }
    fn link(
        &self,
        _linker: &mut Linker<HostState<LocalTypes>>,
    ) -> Result<(), nexum_runtime_api::ExtensionError> {
        Ok(())
    }
    fn emits_trigger_kinds(&self) -> &'static [&'static str] {
        &["ticks"]
    }
}

/// The wall clock sits 3 s past the block.
#[test]
fn a_dispatched_block_records_its_wait_and_its_age() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let clock = wall_clock_at(3);
    let (dispatched, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .clock(clock.as_override())
                .wasm(wasm)
                .module(
                    TestManifest::new("module-a")
                        .cap("logging")
                        .block_trigger(1),
                )
                .boot()
                .await
                .expect("boot");
            let received = received_a_while_ago().await;
            booted
                .supervisor
                .dispatch_block_received(block_at(BLOCK_TIME), received)
                .await
        })
    });
    assert_eq!(dispatched, 1);
    assert_eq!(label_keys(&samples, WAIT), ["module", "trigger_kind"]);
    assert_eq!(
        observed(
            &samples,
            WAIT,
            &[("module", "module-a"), ("trigger_kind", "block")]
        ),
        [QUEUED.as_secs_f64()],
        "{samples:?}",
    );
    assert_eq!(
        label_keys(&samples, AGE),
        ["module", "trigger_kind", "chain_id"]
    );
    assert_eq!(
        observed(
            &samples,
            AGE,
            &[
                ("module", "module-a"),
                ("trigger_kind", "block"),
                ("chain_id", "1"),
            ],
        ),
        [3.0],
        "{samples:?}",
    );
}

/// A node's clock ahead of the host's must not record a negative age, and an
/// unsigned subtraction must not wrap into a huge one.
#[test]
fn a_block_stamped_ahead_of_the_wall_clock_has_zero_age() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let clock = wall_clock_at(-5);
    let (dispatched, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .clock(clock.as_override())
                .wasm(wasm)
                .module(
                    TestManifest::new("module-a")
                        .cap("logging")
                        .block_trigger(1),
                )
                .boot()
                .await
                .expect("boot");
            booted.supervisor.dispatch_block(block_at(BLOCK_TIME)).await
        })
    });
    assert_eq!(dispatched, 1);
    let ages = observed(&samples, AGE, &[]);
    assert_eq!(ages, [0.0], "{samples:?}");
    assert!(ages[0].is_sign_positive(), "{ages:?}");
}

/// The receipt is fixed per block, so the module after slow-host in
/// `[[modules]]` order waits out slow-host's handler as well.
#[test]
fn a_later_module_in_the_fan_out_waits_out_the_earlier_handler() {
    let Some(slow_wasm) = module_wasm_or_skip("slow-host") else {
        return;
    };
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };
    let node = FakeNode::new();
    node.on_method(ChainMethod::EthBlockNumber, "\"0x1\"");
    let (dispatched, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = BootScenario::over(mock_components_from(&node, MockStateStore::new()))
                .module(
                    Entry::new(workspace_manifest(
                        "modules/fixtures/slow-host/component.toml",
                    ))
                    .wasm(slow_wasm),
                )
                .module(
                    Entry::new(TestManifest::new("example").cap("logging").block_trigger(1))
                        .wasm(example_wasm),
                )
                .boot()
                .await
                .expect("boot");
            let received = received_a_while_ago().await;
            node.delay_next_request(PARKED);
            booted
                .supervisor
                .dispatch_block_received(block_at(BLOCK_TIME), received)
                .await
        })
    });
    assert_eq!(dispatched, 2);
    let first = observed(&samples, WAIT, &[("module", "slow-host")]);
    assert_eq!(first, [QUEUED.as_secs_f64()], "{samples:?}");
    let handler = observed(&samples, HANDLER, &[("module", "slow-host")]);
    assert_eq!(handler.len(), 1, "{samples:?}");
    assert!(handler[0] >= PARKED.as_secs_f64(), "{handler:?}");
    let second = observed(&samples, WAIT, &[("module", "example")]);
    assert_eq!(second.len(), 1, "{samples:?}");
    assert_adds_up(
        second[0],
        [first[0], handler[0]],
        "the second module's wait is the first module's wait plus its handler",
    );
}

/// A log's block timestamp is in seconds, where a block's is in milliseconds.
#[test]
fn an_event_with_a_block_timestamp_records_its_wait_and_its_age() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let clock = wall_clock_at(7);
    let (accepted, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .clock(clock.as_override())
                .wasm(wasm)
                .module(
                    TestManifest::new("module-a")
                        .cap("logging")
                        .event_trigger(1),
                )
                .boot()
                .await
                .expect("boot");
            let log = alloy_rpc_types_eth::Log {
                block_number: Some(19_000_000),
                block_timestamp: Some(BLOCK_TIME),
                ..Default::default()
            };
            let received = received_a_while_ago().await;
            booted
                .supervisor
                .dispatch_event_received(
                    &module_id("module-a"),
                    Chain::mainnet(),
                    log,
                    None,
                    received,
                )
                .await
        })
    });
    assert!(accepted);
    assert_eq!(
        observed(
            &samples,
            WAIT,
            &[("module", "module-a"), ("trigger_kind", "event")]
        ),
        [QUEUED.as_secs_f64()],
        "{samples:?}",
    );
    assert_eq!(
        observed(
            &samples,
            AGE,
            &[
                ("module", "module-a"),
                ("trigger_kind", "event"),
                ("chain_id", "1"),
            ],
        ),
        [7.0],
        "{samples:?}",
    );
}

/// A node that reports no `blockTimestamp` leaves the age unknown, and an
/// unknown age is no observation rather than a zero.
#[test]
fn an_event_without_a_block_timestamp_records_no_age() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let (accepted, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .wasm(wasm)
                .module(
                    TestManifest::new("module-a")
                        .cap("logging")
                        .event_trigger(1),
                )
                .boot()
                .await
                .expect("boot");
            let log = alloy_rpc_types_eth::Log {
                block_number: Some(19_000_000),
                ..Default::default()
            };
            booted
                .supervisor
                .dispatch_event(&module_id("module-a"), Chain::mainnet(), log, None)
                .await
        })
    });
    assert!(accepted);
    assert_eq!(
        observed(&samples, WAIT, &[("trigger_kind", "event")]).len(),
        1,
        "{samples:?}",
    );
    assert!(samples_named(&samples, AGE).is_empty(), "{samples:?}");
}

/// The kind label is the extension's own fixed string, and a delivery has no
/// block behind it.
#[test]
fn an_extension_delivery_records_its_wait_under_its_kind_and_no_age() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let (dispatched, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .extensions([Arc::new(Ticks) as Arc<dyn Extension<LocalTypes>>])
                .wasm(wasm)
                .module(
                    TestManifest::new("module-a")
                        .cap("logging")
                        .extension_trigger("ticks", &[]),
                )
                .boot()
                .await
                .expect("boot");
            let delivery = ExtensionDelivery {
                extension_kind: "ticks",
                attrs: Vec::new(),
                trigger: nexum::host::types::Trigger::Extension(
                    nexum::host::types::ExtensionTrigger {
                        extension_kind: "ticks".to_owned(),
                        payload: Vec::new(),
                    },
                ),
            };
            let received = received_a_while_ago().await;
            booted
                .supervisor
                .dispatch_extension_trigger_received(delivery, received)
                .await
        })
    });
    assert_eq!(dispatched, 1);
    assert_eq!(
        observed(
            &samples,
            WAIT,
            &[("module", "module-a"), ("trigger_kind", "ticks")]
        ),
        [QUEUED.as_secs_f64()],
        "{samples:?}",
    );
    assert!(samples_named(&samples, AGE).is_empty(), "{samples:?}");
}

/// A schedule tick waits from the instant its source generated it, and has
/// no block behind it.
#[test]
fn a_schedule_tick_records_its_wait_and_no_age() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let (accepted, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .wasm(wasm)
                .module(
                    TestManifest::new("module-a")
                        .cap("logging")
                        .schedule_trigger("* * * * *"),
                )
                .boot()
                .await
                .expect("boot");
            let received = received_a_while_ago().await;
            booted
                .supervisor
                .dispatch_schedule_received(&module_id("module-a"), 1_715_947_260_000, received)
                .await
        })
    });
    assert!(accepted);
    assert_eq!(
        observed(
            &samples,
            WAIT,
            &[("module", "module-a"), ("trigger_kind", "schedule")]
        ),
        [QUEUED.as_secs_f64()],
        "{samples:?}",
    );
    assert!(samples_named(&samples, AGE).is_empty(), "{samples:?}");
}

/// Both measures are taken at handler start, so a trigger the rate limit
/// drops before the guest contributes to neither.
#[test]
fn a_trigger_dropped_before_the_guest_records_neither_measure() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let (dispatched, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .wasm(wasm)
                .limits(limits_with(|limits| {
                    limits.dispatch = DispatchLimitsSection {
                        burst: Some(1),
                        refill_per_sec: Some(1),
                        ..Default::default()
                    }
                }))
                .module(
                    TestManifest::new("module-a")
                        .cap("logging")
                        .block_trigger(1),
                )
                .boot()
                .await
                .expect("boot");
            // Paused, so the bucket refills nothing between the two.
            tokio::time::pause();
            let first = booted.dispatch_block_on(1).await;
            let second = booted.dispatch_block_on(1).await;
            (first, second)
        })
    });
    assert_eq!(dispatched, (1, 0), "the second block is rate limited");
    assert_eq!(observed(&samples, WAIT, &[]).len(), 1, "{samples:?}");
    assert_eq!(observed(&samples, AGE, &[]).len(), 1, "{samples:?}");
}

/// The loop must hand the handler the stamp the source took, not one of its
/// own: both items below were received [`QUEUED`] before the loop first ran.
#[test]
fn the_event_loop_carries_the_source_receipt_to_the_handler() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let ((blocks, events), samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .wasm(wasm)
                .module(
                    TestManifest::new("module-a")
                        .cap("logging")
                        .block_trigger(1),
                )
                .boot()
                .await
                .expect("boot");
            let received = received_a_while_ago().await;
            let header = alloy_rpc_types_eth::Header::default();
            let block_stream: TaggedBlockStream = Box::pin(
                futures::stream::iter([Ok((Chain::mainnet(), header, received))])
                    .chain(futures::stream::pending()),
            );
            let log = Box::new(alloy_rpc_types_eth::Log::default());
            let log_stream: TaggedChainLogStream = Box::pin(
                futures::stream::iter([(
                    module_id("module-a"),
                    Chain::mainnet(),
                    ChainLogItem::Log(log, received),
                    None,
                )])
                .chain(futures::stream::pending()),
            );
            let outcome = run(
                &mut booted.supervisor,
                vec![block_stream],
                vec![log_stream],
                Vec::new(),
                Vec::new(),
                TaskSet::new(),
                tokio::time::sleep(Duration::from_secs(1)),
            )
            .await;
            (outcome.dispatched_blocks, outcome.dispatched_events)
        })
    });
    assert_eq!((blocks, events), (1, 1));
    for kind in ["block", "event"] {
        assert_eq!(
            observed(&samples, WAIT, &[("trigger_kind", kind)]),
            [QUEUED.as_secs_f64()],
            "{kind}: {samples:?}",
        );
    }
}

/// The block source stamps a header as it arrives, so a block that arrives
/// while a handler is running records the time it then spends queued. Block 2
/// arrives 500 ms into a handler parked for [`PARKED`].
#[test]
fn a_block_queued_behind_a_running_handler_records_the_time_it_queued() {
    let Some(wasm) = module_wasm_or_skip("slow-host") else {
        return;
    };
    const POLL: Duration = Duration::from_millis(20);
    const ARRIVES: Duration = Duration::from_millis(500);
    let module_node = FakeNode::new();
    module_node.on_method(ChainMethod::EthBlockNumber, "\"0x1\"");
    let source_node = FakeNode::new();
    let (dispatched, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted =
                BootScenario::over(mock_components_from(&module_node, MockStateStore::new()))
                    .wasm(wasm)
                    .module(workspace_manifest(
                        "modules/fixtures/slow-host/component.toml",
                    ))
                    .boot()
                    .await
                    .expect("slow-host boots");
            tokio::time::pause();
            module_node.delay_next_request(PARKED);

            let manager = TaskManager::new();
            let executor = manager.executor();
            let mut tasks = TaskSet::new();
            let pool = source_node.pool(&[Chain::mainnet()], POLL);
            source_node.push_block(header_at(1));
            let block_streams =
                open_block_streams(&pool, &[Chain::mainnet()], &executor, &mut tasks);
            let second_block = async {
                tokio::time::sleep(ARRIVES).await;
                source_node.push_block(header_at(2));
            };
            let (outcome, ()) = tokio::join!(
                run(
                    &mut booted.supervisor,
                    block_streams,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    tasks,
                    tokio::time::sleep(PARKED * 3),
                ),
                second_block,
            );
            outcome.dispatched_blocks
        })
    });
    assert_eq!(dispatched, 2);
    let waits = observed(&samples, WAIT, &[("module", "slow-host")]);
    assert_eq!(waits.len(), 2, "{samples:?}");
    assert!(
        waits[0] < POLL.as_secs_f64(),
        "block 1 met an idle loop: {waits:?}",
    );
    let queued = (PARKED - ARRIVES).as_secs_f64();
    assert!(
        (queued - 2.0 * POLL.as_secs_f64()..=queued).contains(&waits[1]),
        "block 2 queued until the parked handler returned: {waits:?}",
    );
}

/// The log source stamps a batch once, so the second log of a block waits out
/// the first log's handler.
#[test]
fn the_second_log_of_a_batch_waits_out_the_first_handler() {
    let Some(wasm) = module_wasm_or_skip("slow-host") else {
        return;
    };
    let module_node = FakeNode::new();
    module_node.on_method(ChainMethod::EthBlockNumber, "\"0x1\"");
    let source_node = FakeNode::new();
    let (dispatched, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted =
                BootScenario::over(mock_components_from(&module_node, MockStateStore::new()))
                    .wasm(wasm)
                    .module(workspace_manifest(
                        "modules/fixtures/slow-host/component.toml",
                    ))
                    .boot()
                    .await
                    .expect("slow-host boots");
            tokio::time::pause();
            module_node.delay_next_request(PARKED);

            let manager = TaskManager::new();
            let executor = manager.executor();
            let mut tasks = TaskSet::new();
            let pool = source_node.pool(&[Chain::mainnet()], Duration::from_millis(20));
            for _ in 0..2 {
                source_node.push_chain_log(alloy_rpc_types_eth::Log {
                    block_number: Some(1),
                    ..Default::default()
                });
            }
            let sources = vec![EventSource {
                module: module_id("slow-host"),
                chain: Chain::mainnet(),
                filter: alloy_rpc_types_eth::Filter::default(),
                cursor_key: None,
                initial_cursor: None,
                max_lookback: None,
            }];
            let chain_log_streams = open_chain_log_streams(&pool, sources, &executor, &mut tasks);
            run(
                &mut booted.supervisor,
                Vec::new(),
                chain_log_streams,
                Vec::new(),
                Vec::new(),
                tasks,
                tokio::time::sleep(PARKED * 3),
            )
            .await
            .dispatched_events
        })
    });
    assert_eq!(dispatched, 2);
    let waits = observed(
        &samples,
        WAIT,
        &[("module", "slow-host"), ("trigger_kind", "event")],
    );
    assert_eq!(waits.len(), 2, "{samples:?}");
    assert!(waits[0] < 0.001, "log 1 met an idle loop: {waits:?}");
    let handlers = observed(&samples, HANDLER, &[("module", "slow-host")]);
    assert_eq!(handlers.len(), 2, "{samples:?}");
    assert!(handlers[0] >= PARKED.as_secs_f64(), "{handlers:?}");
    assert_adds_up(
        waits[1],
        [waits[0], handlers[0]],
        "log 2's wait is log 1's wait plus log 1's handler",
    );
}

fn header_at(number: u64) -> alloy_rpc_types_eth::Header {
    let mut header: alloy_rpc_types_eth::Header = Default::default();
    header.inner.number = number;
    header
}
