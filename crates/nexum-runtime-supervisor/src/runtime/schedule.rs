//! Schedule sources: one task per live `schedule` trigger.
//!
//! A task asks its cron expression for the next fire time after the current
//! wall clock, in UTC, and sleeps towards it on the runtime clock. It reads
//! the wall clock again at least once a second, so a step in either
//! direction moves the schedule with the clock: one tick at most for
//! whatever a forward step skipped, and no burst.
//!
//! Nothing is persisted, so a boot starts from the present and replays
//! nothing. Each source holds at most one undelivered tick: one that comes
//! due while the event loop is still busy waits in a one-slot channel, and
//! a later one is dropped and counted while that slot is full. The waiting
//! tick keeps the `fired_at` it was generated with. That bound lets the
//! loop select a tick ahead of the chain sources without starving them.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, error, info};

use crate::manifest::CronSchedule;
use crate::supervisor::{ReceivedAt, ScheduleSource};
use nexum_primitives::module_id::ModuleId;
use nexum_runtime_api::HostWallClock;
use nexum_tasks::{TaskExecutor, TaskExit, TaskSet};

// One spelling for the `trigger_kind` metric label, the task label and the
// log field.
pub(crate) const SOURCE_KIND_SCHEDULE: &str = "schedule";

/// Longest sleep before the wall clock is read again; it bounds how late a
/// tick is after the wall clock steps forward.
const WALL_CLOCK_RECHECK: Duration = Duration::from_secs(1);

/// Undelivered ticks one source holds; a later tick coalesces into it.
const PENDING_TICKS: usize = 1;

/// One tick of one module's schedule trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleTick {
    /// Module whose trigger fired.
    pub module: ModuleId,
    /// Host wall-clock time the tick was generated at, in milliseconds
    /// since the Unix epoch.
    pub fired_at: u64,
    /// The instant the source generated the tick, for the delivery measure.
    pub received: ReceivedAt,
}

/// Stream of [`ScheduleTick`] from one schedule source.
pub type ScheduleStream = Pin<Box<dyn Stream<Item = ScheduleTick> + Send>>;

/// Open one task per schedule source, spawned via `executor` with handles
/// pushed into `tasks` for graceful shutdown. `wall` is the clock every
/// source reads; no chain is involved.
pub fn open_schedule_streams(
    sources: Vec<ScheduleSource>,
    wall: Arc<dyn HostWallClock + Send + Sync>,
    executor: &TaskExecutor,
    tasks: &mut TaskSet,
) -> Vec<ScheduleStream> {
    let mut streams = Vec::new();
    for source in sources {
        let (tx, rx) = mpsc::channel::<ScheduleTick>(PENDING_TICKS);
        tasks.push(
            format!("{SOURCE_KIND_SCHEDULE}:{}:{}", source.module, source.cron),
            executor.spawn(schedule_task(source.module, source.cron, wall.clone(), tx)),
        );
        let stream: ScheduleStream = Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|tick| (tick, rx))
        }));
        streams.push(stream);
    }
    streams
}

/// Loop for one schedule source; ends when its receiver drops.
async fn schedule_task(
    module: ModuleId,
    cron: CronSchedule,
    wall: Arc<dyn HostWallClock + Send + Sync>,
    tx: mpsc::Sender<ScheduleTick>,
) -> TaskExit {
    info!(
        module = %module,
        cron = %cron,
        source_kind = SOURCE_KIND_SCHEDULE,
        "schedule source open"
    );
    loop {
        // Armed from the current wall clock on every pass, never from the
        // last fire time: an instant the clock skipped is not owed.
        let armed_at = wall.now();
        let Some(due) = cron.next_after(armed_at) else {
            error!(
                module = %module,
                cron = %cron,
                source_kind = SOURCE_KIND_SCHEDULE,
                "schedule has no next fire time - source parked",
            );
            tx.closed().await;
            return TaskExit::ReceiverGone;
        };
        let wait = due.saturating_sub(armed_at).min(WALL_CLOCK_RECHECK);
        tokio::select! {
            () = tx.closed() => return TaskExit::ReceiverGone,
            () = tokio::time::sleep(wait) => {}
        }
        let now = wall.now();
        if now < due {
            continue;
        }
        let tick = ScheduleTick {
            module: module.clone(),
            fired_at: u64::try_from(now.as_millis()).unwrap_or(u64::MAX),
            received: ReceivedAt::now(),
        };
        match tx.try_send(tick) {
            Ok(()) => {}
            Err(TrySendError::Full(tick)) => {
                debug!(
                    module = %module,
                    cron = %cron,
                    fired_at = tick.fired_at,
                    "schedule tick still pending - coalescing this one into it",
                );
                metrics::counter!(
                    "nexum_runtime_dispatch_dropped_total",
                    "module" => module.to_string(),
                    "trigger_kind" => SOURCE_KIND_SCHEDULE,
                    "reason" => "coalesced",
                )
                .increment(1);
            }
            Err(TrySendError::Closed(_)) => return TaskExit::ReceiverGone,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use futures::{FutureExt, StreamExt};
    use nexum_tasks::TaskManager;

    use super::*;
    use crate::event_loop::{RunEnd, run};
    use crate::test_utils::{
        BootScenario, LocalTypes, ManualClock, TestManifest, block_on_current_thread,
        capture_metrics, limits_with, module_wasm_or_skip, samples_named,
    };

    /// 2024-05-17T12:00:00Z, a Friday.
    const NOON: u64 = 1_715_947_200;

    fn clock_at(secs_past_noon: u64) -> ManualClock {
        let clock = ManualClock::new();
        clock.set(UNIX_EPOCH + Duration::from_secs(NOON + secs_past_noon));
        clock
    }

    /// Milliseconds since the epoch, `secs` past [`NOON`].
    fn ms_past_noon(secs: u64) -> u64 {
        (NOON + secs) * 1_000
    }

    fn source(module: &str, cron: &str) -> ScheduleSource {
        ScheduleSource {
            module: ModuleId::parse(module).expect("a plain name parses"),
            cron: cron.parse().expect("a valid cron expression"),
        }
    }

    struct Opened {
        streams: Vec<ScheduleStream>,
        tasks: TaskSet,
        // Dropping the manager fires shutdown; hold it for the test.
        _manager: TaskManager,
    }

    fn open(clock: &ManualClock, sources: Vec<ScheduleSource>) -> Opened {
        let manager = TaskManager::new();
        let mut tasks = TaskSet::new();
        let streams = open_schedule_streams(
            sources,
            Arc::new(clock.clone()),
            &manager.executor(),
            &mut tasks,
        );
        Opened {
            streams,
            tasks,
            _manager: manager,
        }
    }

    /// Let the source tasks run to their next await.
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// Move the wall clock and the runtime clock together, a second at a
    /// time so a source sees every instant on the way.
    async fn pass(clock: &ManualClock, secs: u64) {
        settle().await;
        for _ in 0..secs {
            clock.advance(Duration::from_secs(1));
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
        }
    }

    /// Every tick already waiting on `stream`, without advancing time.
    fn drain(stream: &mut ScheduleStream) -> Vec<u64> {
        let mut fired = Vec::new();
        while let Some(Some(tick)) = stream.next().now_or_never() {
            fired.push(tick.fired_at);
        }
        fired
    }

    /// [`pass`], draining `stream` after every second.
    async fn pass_draining(
        clock: &ManualClock,
        stream: &mut ScheduleStream,
        secs: u64,
    ) -> Vec<u64> {
        let mut fired = Vec::new();
        for _ in 0..secs {
            pass(clock, 1).await;
            fired.extend(drain(stream));
        }
        fired
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_fires_at_each_matching_minute_and_at_no_other_instant() {
        let clock = clock_at(20);
        let mut opened = open(&clock, vec![source("m", "*/2 * * * *")]);
        let stream = &mut opened.streams[0];

        // 12:00:20 to 12:01:59: the odd minute passes without a tick.
        assert_eq!(pass_draining(&clock, stream, 99).await, Vec::<u64>::new());
        // 12:02:00 exactly.
        assert_eq!(pass_draining(&clock, stream, 1).await, [ms_past_noon(120)]);
        // The next four minutes carry two more, on the even minutes.
        assert_eq!(
            pass_draining(&clock, stream, 240).await,
            [ms_past_noon(240), ms_past_noon(360)],
        );
    }

    /// The boot instant is not a tick, even on a matching minute boundary.
    #[tokio::test(start_paused = true)]
    async fn a_source_opened_on_a_matching_minute_waits_for_the_next_one() {
        let clock = clock_at(0);
        let mut opened = open(&clock, vec![source("m", "* * * * *")]);
        let stream = &mut opened.streams[0];
        assert_eq!(pass_draining(&clock, stream, 59).await, Vec::<u64>::new());
        assert_eq!(pass_draining(&clock, stream, 1).await, [ms_past_noon(60)]);
    }

    #[tokio::test(start_paused = true)]
    async fn each_schedule_trigger_gets_its_own_source() {
        let clock = clock_at(30);
        let mut opened = open(
            &clock,
            vec![
                source("m", "* * * * *"),
                source("m", "*/2 * * * *"),
                source("other", "1 12 * * *"),
            ],
        );
        assert_eq!(opened.streams.len(), 3);
        pass(&clock, 30).await;
        assert_eq!(drain(&mut opened.streams[0]), [ms_past_noon(60)]);
        assert_eq!(drain(&mut opened.streams[1]), Vec::<u64>::new());
        let mut third = opened.streams.remove(2);
        let tick = third.next().now_or_never().flatten().expect("12:01 is due");
        assert_eq!(tick.module.as_str(), "other");
        assert_eq!(tick.fired_at, ms_past_noon(60));
    }

    /// A forward step past many fire times yields one tick, stamped with
    /// the stepped clock, and the schedule resumes from there.
    #[tokio::test(start_paused = true)]
    async fn a_forward_clock_step_yields_one_tick_and_no_burst() {
        let clock = clock_at(30);
        let mut opened = open(&clock, vec![source("m", "*/5 * * * *")]);
        let stream = &mut opened.streams[0];
        assert_eq!(pass_draining(&clock, stream, 10).await, Vec::<u64>::new());

        // 12:00:40 steps to 15:00:40: thirty-six fire times are skipped.
        clock.advance(Duration::from_secs(3 * 3_600));
        let stepped = 40 + 3 * 3_600;
        assert_eq!(
            pass_draining(&clock, stream, 1).await,
            [ms_past_noon(stepped + 1)],
            "one tick, generated on the first wake after the step",
        );
        // Nothing more until 15:05:00.
        assert_eq!(pass_draining(&clock, stream, 258).await, Vec::<u64>::new());
        assert_eq!(
            pass_draining(&clock, stream, 1).await,
            [ms_past_noon(3 * 3_600 + 300)],
        );
    }

    /// A backward step re-arms from the stepped clock: no tick fires until
    /// the wall clock reaches a matching minute again, and it then fires
    /// once for that minute even though the minute already fired before.
    #[tokio::test(start_paused = true)]
    async fn a_backward_clock_step_follows_the_wall_clock() {
        let clock = clock_at(30);
        let mut opened = open(&clock, vec![source("m", "* * * * *")]);
        let stream = &mut opened.streams[0];
        assert_eq!(pass_draining(&clock, stream, 35).await, [ms_past_noon(60)]);

        // 12:01:05 steps back to 12:00:10.
        clock.set(UNIX_EPOCH + Duration::from_secs(NOON + 10));
        assert_eq!(pass_draining(&clock, stream, 49).await, Vec::<u64>::new());
        assert_eq!(pass_draining(&clock, stream, 1).await, [ms_past_noon(60)]);
        assert_eq!(pass_draining(&clock, stream, 60).await, [ms_past_noon(120)]);
    }

    /// A wall clock that stands still while the runtime clock runs produces
    /// no tick and does not spin: the source wakes once per recheck.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_wall_clock_fires_nothing() {
        let clock = clock_at(59);
        let mut opened = open(&clock, vec![source("m", "* * * * *")]);
        let waited = tokio::time::timeout(Duration::from_secs(3_600), opened.streams[0].next());
        assert!(waited.await.is_err(), "no tick without the wall clock");
    }

    /// The consumer stops reading for five fire times. One tick waits, the
    /// first; the other four are dropped and counted, and the schedule
    /// then carries on from the present.
    #[test]
    fn ticks_pending_for_a_slow_consumer_coalesce_to_one() {
        let (fired, samples) = capture_metrics(|| {
            block_on_current_thread(async {
                tokio::time::pause();
                let clock = clock_at(30);
                let mut opened = open(&clock, vec![source("slow", "* * * * *")]);
                // 12:00:30 to 12:05:30 with nothing reading the stream.
                pass(&clock, 300).await;
                let mut fired = drain(&mut opened.streams[0]);
                fired.extend(pass_draining(&clock, &mut opened.streams[0], 30).await);
                fired
            })
        });
        assert_eq!(
            fired,
            [ms_past_noon(60), ms_past_noon(360)],
            "the first missed tick, then the next one from the present",
        );
        let dropped = samples_named(&samples, "nexum_runtime_dispatch_dropped_total");
        assert_eq!(dropped.len(), 1, "one series: {samples:?}");
        assert!(
            dropped[0].has_label("reason", "coalesced")
                && dropped[0].has_label("module", "slow")
                && dropped[0].has_label("trigger_kind", "schedule"),
            "{:?}",
            dropped[0].labels,
        );
        assert!(
            matches!(
                dropped[0].value,
                crate::test_utils::metrics_util::debugging::DebugValue::Counter(4)
            ),
            "{:?}",
            dropped[0].value,
        );
    }

    /// An hour passes with the engine down. The reopened source holds no
    /// memory of the old one and replays none of the sixty missed ticks.
    #[tokio::test(start_paused = true)]
    async fn a_restart_replays_nothing() {
        let clock = clock_at(30);
        let mut opened = open(&clock, vec![source("m", "* * * * *")]);
        assert_eq!(
            pass_draining(&clock, &mut opened.streams[0], 90).await,
            [ms_past_noon(60), ms_past_noon(120)],
        );
        let Opened { streams, tasks, .. } = opened;
        drop(streams);
        tasks.shutdown().await;

        // 12:02:00 becomes 13:02:00 while nothing runs.
        clock.advance(Duration::from_secs(3_600));
        let mut reopened = open(&clock, vec![source("m", "* * * * *")]);
        let stream = &mut reopened.streams[0];
        assert_eq!(pass_draining(&clock, stream, 59).await, Vec::<u64>::new());
        assert_eq!(
            pass_draining(&clock, stream, 1).await,
            [ms_past_noon(3_600 + 180)],
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_ends_cleanly_when_its_stream_drops() {
        let clock = clock_at(30);
        let Opened {
            streams,
            mut tasks,
            _manager,
        } = open(
            &clock,
            vec![source("m", "* * * * *"), source("m", "0 0 29 2 *")],
        );
        settle().await;
        drop(streams);
        for _ in 0..2 {
            let exit = tokio::time::timeout(Duration::from_secs(5), tasks.join_next())
                .await
                .expect("a source notices its receiver is gone");
            assert_eq!(exit, TaskExit::ReceiverGone);
        }
        assert!(tasks.died().is_empty(), "no source was aborted");
        tasks.shutdown().await;
    }

    fn records(booted: &crate::test_utils::Booted<LocalTypes>, module: &str) -> Vec<String> {
        booted
            .records(module)
            .into_iter()
            .map(|record| record.message)
            .filter(|message| message.starts_with("tick "))
            .collect()
    }

    /// The whole path below the launcher: a schedule-only supervisor with
    /// no chain configured, real sources, and the event loop dispatching
    /// into a real guest until shutdown.
    #[tokio::test(start_paused = true)]
    async fn run_dispatches_schedule_ticks_to_a_chainless_engine() {
        let Some(wasm) = module_wasm_or_skip("tick-recorder") else {
            return;
        };
        let clock = clock_at(30);
        let mut booted = BootScenario::<LocalTypes>::new()
            .chains(std::collections::HashMap::new())
            .clock(clock.as_override())
            .wasm(wasm)
            .module(
                TestManifest::new("tick-recorder")
                    .cap("logging")
                    .schedule_trigger("* * * * *"),
            )
            .boot()
            .await
            .expect("a schedule-only module boots without a [chains] entry");
        let plan = booted.supervisor.source_plan();
        assert_eq!(plan.schedule_sources.len(), 1);

        let manager = TaskManager::new();
        let mut tasks = TaskSet::new();
        let streams = open_schedule_streams(
            plan.schedule_sources,
            booted.supervisor.wall_clock(),
            &manager.executor(),
            &mut tasks,
        );
        // 12:00:30 to 12:03:30, then stop: three minutes, three ticks.
        let shutdown = pass(&clock, 180);
        let outcome = run(
            &mut booted.supervisor,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            streams,
            tasks,
            shutdown,
        )
        .await;
        assert!(matches!(outcome.end, RunEnd::Shutdown), "{:?}", outcome.end);
        assert_eq!(
            records(&booted, "tick-recorder"),
            [
                format!("tick {}", ms_past_noon(60)),
                format!("tick {}", ms_past_noon(120)),
                format!("tick {}", ms_past_noon(180)),
            ],
        );
        assert_eq!(booted.supervisor.alive_count(), 1);
    }

    /// A handler that outlasts several fire times gets one tick for them,
    /// not one each. The guest holds every dispatch for 250 s, and the
    /// event loop is serial, so the ticks that come due meanwhile meet a
    /// full slot: the first waits with its own `fired_at`, the rest drop.
    #[tokio::test(start_paused = true)]
    async fn a_slow_handler_gets_one_tick_for_the_fire_times_it_outlasted() {
        let Some(wasm) = module_wasm_or_skip("tick-recorder") else {
            return;
        };
        let clock = clock_at(30);
        let mut booted = BootScenario::<LocalTypes>::new()
            .chains(std::collections::HashMap::new())
            .clock(clock.as_override())
            .limits(limits_with(|limits| {
                limits.dispatch.deadline_secs = Some(3_600);
            }))
            .wasm(wasm)
            .module(
                TestManifest::new("tick-recorder")
                    .cap("logging")
                    .schedule_trigger("* * * * *")
                    .config("hold_ms", "250000"),
            )
            .boot()
            .await
            .expect("boot");
        let manager = TaskManager::new();
        let mut tasks = TaskSet::new();
        let streams = open_schedule_streams(
            booted.supervisor.source_plan().schedule_sources,
            booted.supervisor.wall_clock(),
            &manager.executor(),
            &mut tasks,
        );

        // Time passes on its own task: the loop cannot poll its shutdown
        // future while a dispatch is in flight. 12:00:30 to 12:09:45.
        let driver = {
            let clock = clock.clone();
            manager
                .executor()
                .spawn(async move { pass(&clock, 555).await })
        };
        let outcome = run(
            &mut booted.supervisor,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            streams,
            tasks,
            driver.join(),
        )
        .await;
        assert!(matches!(outcome.end, RunEnd::Shutdown), "{:?}", outcome.end);
        // 12:01 runs until 12:05:10. 12:02 waits; 12:03 to 12:05 drop.
        // 12:02 runs until 12:09:20. 12:06 waits; 12:07 to 12:09 drop.
        assert_eq!(
            records(&booted, "tick-recorder"),
            [
                format!("tick {}", ms_past_noon(60)),
                format!("tick {}", ms_past_noon(120)),
                format!("tick {}", ms_past_noon(360)),
            ],
            "ten fire times, three dispatches",
        );
        assert_eq!(booted.supervisor.alive_count(), 1);
    }

    /// The loop polls schedule sources ahead of the chain arms. The select
    /// is biased, so a chain-log backlog that is never empty when the loop
    /// returns to it would otherwise hold every tick until it drained. Each
    /// log dispatch here holds for 100 ms, which lets the feeder refill
    /// the channel before the loop looks again.
    #[tokio::test(start_paused = true)]
    async fn a_chain_log_backlog_does_not_hold_a_schedule_tick() {
        use alloy_chains::Chain;

        use crate::event_loop::{ChainLogItem, TaggedChainLog, TaggedChainLogStream};

        let Some(wasm) = module_wasm_or_skip("tick-recorder") else {
            return;
        };
        let clock = clock_at(30);
        let mut booted = BootScenario::<LocalTypes>::new()
            .clock(clock.as_override())
            .wasm(wasm)
            .module(
                TestManifest::new("tick-recorder")
                    .cap("logging")
                    .schedule_trigger("* * * * *")
                    .event_trigger(1)
                    .config("hold_ms", "100"),
            )
            .boot()
            .await
            .expect("boot");
        let manager = TaskManager::new();
        let mut tasks = TaskSet::new();
        let streams = open_schedule_streams(
            booted.supervisor.source_plan().schedule_sources,
            booted.supervisor.wall_clock(),
            &manager.executor(),
            &mut tasks,
        );

        let (flood_tx, flood_rx) = mpsc::channel::<TaggedChainLog>(64);
        tasks.push(
            "chain-log:1:tick-recorder",
            manager.executor().spawn(async move {
                let module = ModuleId::parse("tick-recorder").expect("a plain name parses");
                let log = || ChainLogItem::Log(Box::default(), ReceivedAt::now());
                while flood_tx
                    .send((module.clone(), Chain::mainnet(), log(), None))
                    .await
                    .is_ok()
                {}
                TaskExit::ReceiverGone
            }),
        );
        let flood: TaggedChainLogStream =
            Box::pin(futures::stream::unfold(flood_rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }));

        // 12:00:30 to 12:01:50, on its own task: the loop is mid-dispatch
        // nearly all the time.
        let driver = {
            let clock = clock.clone();
            manager
                .executor()
                .spawn(async move { pass(&clock, 80).await })
        };
        let outcome = run(
            &mut booted.supervisor,
            Vec::new(),
            vec![flood],
            Vec::new(),
            streams,
            tasks,
            driver.join(),
        )
        .await;
        assert!(matches!(outcome.end, RunEnd::Shutdown), "{:?}", outcome.end);
        // The driver steps a second at a time, so a held dispatch ends on
        // a step: about one log a second for the 80 seconds.
        assert!(
            outcome.dispatched_events >= 60,
            "the backlog was being drained throughout: {}",
            outcome.dispatched_events,
        );
        assert_eq!(
            records(&booted, "tick-recorder"),
            [format!("tick {}", ms_past_noon(60))],
            "the 12:01 tick was dispatched while the backlog was still full",
        );
    }
}
