//! Cross-cutting runtime add-ons: process-wide facilities that attach to the
//! launch path without the core knowing their concrete type. An add-on
//! installs a facility from the resolved config and returns a handle the
//! launcher keeps alive for the run.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder, PrometheusHandle};
use nexum_runtime_supervisor::supervisor::{HealthSnapshot, HealthWatch};
use nexum_tasks::TaskExecutor;
use tracing::{error, info};

use crate::engine_config::MetricsSection;
use crate::error::BoxError;

pub use metrics_exporter_prometheus::BuildError as PrometheusBuildError;
pub use nexum_runtime_metrics::{Kind, METRICS, Metric, describe_all};

/// The foreign cause renders inline, so the operator sees one line.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrometheusError {
    /// `[engine.metrics].bind_addr` is not a socket address.
    #[error("invalid [engine.metrics].bind_addr `{addr}`: {cause}")]
    BindAddr {
        /// The value as the operator wrote it.
        addr: String,
        /// What the address parser objected to.
        cause: std::net::AddrParseError,
    },
    /// The observability port could not be taken, typically because it is
    /// already in use.
    #[error("bind observability listener on {addr}: {cause}")]
    Listener {
        /// The address that was parsed and then refused.
        addr: SocketAddr,
        /// What the bind objected to.
        cause: std::io::Error,
    },
    /// Installing the recorder failed, which leaves every `metrics!` call
    /// site recording into nothing.
    #[error("install Prometheus recorder: {cause}")]
    Recorder {
        /// What the recorder build objected to.
        cause: BuildError,
    },
}

/// Inputs an add-on reads at install time.
pub struct AddOnsContext<'a> {
    /// Resolved `[engine.metrics]` config.
    pub metrics: &'a MetricsSection,
    /// Read side of the supervisor's readiness channel; empty until the
    /// supervisor's first publication, which reads as not-ready.
    pub health: &'a HealthWatch,
    /// Spawns an add-on's own tasks under the run's task lifecycle.
    pub executor: &'a TaskExecutor,
}

/// A live add-on installation, retained by the launcher for the run.
pub struct AddOnHandle {
    /// The add-on's name, for diagnostics.
    pub name: &'static str,
}

impl AddOnHandle {
    /// A handle for an add-on that needs no teardown resource.
    pub fn named(name: &'static str) -> Self {
        Self { name }
    }
}

/// A process-wide facility attached to the launch path.
pub trait RuntimeAddOn {
    /// Install the facility, returning its live handle.
    fn install(&self, ctx: &AddOnsContext<'_>) -> Result<AddOnHandle, BoxError>;
}

/// An owned, ordered add-on set.
pub type AddOns = Vec<Box<dyn RuntimeAddOn>>;

/// The observability add-on. With `[engine.metrics].enabled = true` it binds
/// one listener serving `/metrics`, `/healthz` and `/readyz`; otherwise it
/// installs the recorder alone so `metrics::counter!` call sites stay live but
/// no port opens.
pub struct PrometheusAddOn;

/// What the routes read; the state extractor clones it per request.
#[derive(Clone)]
struct Probe {
    metrics: PrometheusHandle,
    health: HealthWatch,
}

/// Bucket bounds for the dispatch latency histogram, spanning the 5 s
/// alert threshold in `docs/production.md`.
const DISPATCH_LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

// Without explicit buckets the exporter renders a histogram as a quantile
// summary, and the `_bucket` series the latency alert reads never exists.
fn prometheus_builder() -> Result<PrometheusBuilder, BuildError> {
    PrometheusBuilder::new().set_buckets_for_metric(
        Matcher::Full("nexum_runtime_dispatch_latency_seconds".to_owned()),
        DISPATCH_LATENCY_BUCKETS,
    )
}

/// Bound and handed to the reactor before the launch continues, so every way
/// the port can be refused refuses the launch rather than surfacing as a dead
/// endpoint on a running engine. The returned address is the bound one, which
/// a `:0` port resolves.
///
/// Call from inside the tokio runtime the engine runs on.
fn bind(bind_addr: &str) -> Result<(SocketAddr, tokio::net::TcpListener), PrometheusError> {
    let addr: SocketAddr = bind_addr
        .parse()
        .map_err(|cause| PrometheusError::BindAddr {
            addr: bind_addr.to_owned(),
            cause,
        })?;
    let listener = std::net::TcpListener::bind(addr)
        .map_err(|cause| PrometheusError::Listener { addr, cause })?;
    listener
        .set_nonblocking(true)
        .map_err(|cause| PrometheusError::Listener { addr, cause })?;
    let bound = listener.local_addr().unwrap_or(addr);
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|cause| PrometheusError::Listener { addr, cause })?;
    Ok((bound, listener))
}

async fn render_metrics(State(probe): State<Probe>) -> String {
    probe.metrics.render()
}

/// Liveness answers for the process, not for any module: a wedged process
/// stops answering at all, which is the condition a restart fixes.
async fn healthz() -> &'static str {
    "ok\n"
}

async fn readyz(State(probe): State<Probe>) -> (StatusCode, String) {
    let snapshot = probe.health.snapshot();
    let status = if snapshot.ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, readiness_body(&snapshot))
}

/// The aggregate the probe reads, then the per-module detail it flattens.
fn readiness_body(snapshot: &HealthSnapshot) -> String {
    std::iter::once(format!("ready: {}\n", snapshot.ready()))
        .chain(
            snapshot
                .modules()
                .map(|(name, state)| format!("{name}: {}\n", <&str>::from(state))),
        )
        .collect()
}

fn routes(probe: Probe) -> Router {
    Router::new()
        .route("/metrics", get(render_metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(probe)
}

impl RuntimeAddOn for PrometheusAddOn {
    fn install(&self, ctx: &AddOnsContext<'_>) -> Result<AddOnHandle, BoxError> {
        // Bind before installing the recorder: a refused address must leave no
        // global recorder behind. With no listener the recorder still installs
        // and accumulates samples in memory, unread.
        let bound = ctx
            .metrics
            .enabled
            .then(|| bind(&ctx.metrics.bind_addr))
            .transpose()?;
        let handle = prometheus_builder()
            .and_then(PrometheusBuilder::install_recorder)
            .map_err(|cause| PrometheusError::Recorder { cause })?;
        nexum_runtime_metrics::describe_all();
        if let Some((addr, listener)) = bound {
            let app = routes(Probe {
                metrics: handle,
                health: ctx.health.clone(),
            });
            ctx.executor.spawn(async move {
                if let Err(err) = axum::serve(listener, app).await {
                    error!(error = %err, "observability listener stopped");
                }
            });
            info!(
                addr = %addr,
                "observability listening at /metrics, /healthz and /readyz",
            );
        }
        Ok(AddOnHandle::named("prometheus"))
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use nexum_primitives::module_id::ModuleId;
    use nexum_runtime_supervisor::supervisor::{ModuleState, health_channel};
    use nexum_tasks::TaskManager;
    use tower::ServiceExt as _;

    use super::*;
    use crate::engine_config::MetricsSection;
    use crate::test_utils::Refusal;

    /// The `NexumDispatchLatency` alert reads `_bucket` series by `le`, so
    /// the latency metric must render as a Prometheus histogram. Bounds are
    /// matched on the bare name, so call-site labels cannot cost them.
    #[test]
    fn the_latency_histogram_renders_bucket_series() {
        const NAME: &str = "nexum_runtime_dispatch_latency_seconds";
        let recorder = prometheus_builder()
            .expect("a non-empty bucket list builds")
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!(NAME, "module" => "m", "trigger_kind" => "block", "outcome" => "trap")
                .record(0.5);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains(&format!("# TYPE {NAME} histogram")),
            "exposition:\n{rendered}",
        );
        assert!(
            rendered.contains(&format!("{NAME}_bucket{{")) && rendered.contains("le=\"5\""),
            "exposition:\n{rendered}",
        );
    }

    /// An enabled exporter with an unparseable bind address fails at install.
    #[tokio::test]
    async fn prometheus_add_on_rejects_an_invalid_bind_addr() {
        let mut metrics = MetricsSection::default();
        metrics.enabled = true;
        metrics.bind_addr = "not-a-socket-addr".to_owned();
        let tasks = TaskManager::new();
        let executor = tasks.executor();
        let (_publisher, health) = health_channel();
        let ctx = AddOnsContext {
            metrics: &metrics,
            health: &health,
            executor: &executor,
        };
        let err = match PrometheusAddOn.install(&ctx) {
            Ok(_) => panic!("invalid bind_addr must not install"),
            Err(err) => err,
        };
        Refusal::from(crate::error::RuntimeError::AddOn(err)).variant::<PrometheusError>(
            |e| matches!(e, PrometheusError::BindAddr { addr, .. } if addr == "not-a-socket-addr"),
        );
    }

    /// The bind runs before the recorder install, so a taken port refuses the
    /// launch instead of leaving a running engine with a dead endpoint.
    #[tokio::test]
    async fn prometheus_add_on_rejects_a_port_already_in_use() {
        let (taken, _held) = bind("127.0.0.1:0").expect("an ephemeral loopback port");
        let mut metrics = MetricsSection::default();
        metrics.enabled = true;
        metrics.bind_addr = taken.to_string();
        let tasks = TaskManager::new();
        let executor = tasks.executor();
        let (_publisher, health) = health_channel();
        let ctx = AddOnsContext {
            metrics: &metrics,
            health: &health,
            executor: &executor,
        };
        let err = match PrometheusAddOn.install(&ctx) {
            Ok(_) => panic!("a taken port must not install"),
            Err(err) => err,
        };
        Refusal::from(crate::error::RuntimeError::AddOn(err)).variant::<PrometheusError>(
            |e| matches!(e, PrometheusError::Listener { addr, .. } if *addr == taken),
        );
    }

    fn module(name: &str) -> ModuleId {
        ModuleId::parse(name).expect("a valid module name")
    }

    /// The bound socket really serves: the oneshot tests below drive the
    /// router directly and would pass with the reactor handoff broken.
    #[tokio::test]
    async fn the_bound_listener_answers_over_the_socket() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let (addr, listener) = bind("127.0.0.1:0").expect("an ephemeral loopback port");
        let (_publisher, health) = health_channel();
        let app = probe(health);
        let tasks = TaskManager::new();
        tasks.executor().spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n")
            .await
            .expect("write the request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read the response");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("ok\n"), "{response}");
    }

    /// Routes over a local recorder handle, so the test never installs a
    /// global one.
    fn probe(health: HealthWatch) -> Router {
        let recorder = prometheus_builder()
            .expect("a non-empty bucket list builds")
            .build_recorder();
        routes(Probe {
            metrics: recorder.handle(),
            health,
        })
    }

    async fn get_route(app: Router, path: &str) -> (StatusCode, String) {
        let response = app
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("a request with no body"),
            )
            .await
            .expect("the router is infallible");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a fully buffered body");
        (status, String::from_utf8(body.to_vec()).expect("utf-8"))
    }

    /// Every alert rule in `docs/production.md` reads this path; owning the
    /// server must not change what it serves, HELP and TYPE included.
    #[tokio::test]
    async fn metrics_still_renders_the_prometheus_exposition() {
        const NAME: &str = "nexum_runtime_module_restarts_total";
        let recorder = prometheus_builder()
            .expect("a non-empty bucket list builds")
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            nexum_runtime_metrics::describe_all();
            metrics::counter!(NAME, "module" => "m").increment(2);
        });
        let (_publisher, health) = health_channel();
        let app = routes(Probe {
            metrics: handle,
            health,
        });

        let (status, body) = get_route(app, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("# HELP"), "exposition:\n{body}");
        assert!(
            body.contains(&format!("# TYPE {NAME} counter"))
                && body.contains(&format!("{NAME}{{module=\"m\"}} 2")),
            "exposition:\n{body}",
        );
    }

    #[tokio::test]
    async fn healthz_answers_while_the_process_is_up() {
        let (_publisher, health) = health_channel();
        let (status, body) = get_route(probe(health), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok\n");
    }

    #[tokio::test]
    async fn readyz_is_unavailable_before_the_supervisor_publishes() {
        let (_publisher, health) = health_channel();
        let (status, body) = get_route(probe(health), "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "ready: false\n");
    }

    /// One quarantined module must not pull an engine still serving the
    /// others out of rotation, and the flattened detail stays in the body.
    #[tokio::test]
    async fn readyz_is_ready_with_one_alive_module_and_names_every_state() {
        let (publisher, health) = health_channel();
        publisher.publish([
            (module("quarantined"), ModuleState::Poisoned),
            (module("waiting"), ModuleState::Backoff),
            (module("serving"), ModuleState::Alive),
        ]);
        let (status, body) = get_route(probe(health), "/readyz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            "ready: true\nquarantined: poisoned\nwaiting: backoff\nserving: alive\n",
        );
    }

    #[tokio::test]
    async fn readyz_falls_back_to_unavailable_once_every_module_stops_dispatching() {
        let (publisher, health) = health_channel();
        publisher.publish([(module("only"), ModuleState::Alive)]);
        let app = probe(health.clone());
        assert_eq!(get_route(app, "/readyz").await.0, StatusCode::OK);
        publisher.publish([(module("only"), ModuleState::Poisoned)]);
        let (status, body) = get_route(probe(health), "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "ready: false\nonly: poisoned\n");
    }
}
