//! Cross-cutting runtime add-ons: process-wide facilities that attach to the
//! launch path without the core knowing their concrete type. An add-on
//! installs a facility from the resolved config and returns a handle the
//! launcher keeps alive for the run.

use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder};
use tracing::info;

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
    /// The exporter could not take the listener, typically because the
    /// port is in use.
    #[error("install Prometheus exporter on {addr}: {cause}")]
    Exporter {
        /// The address that was parsed and then refused.
        addr: std::net::SocketAddr,
        /// What the exporter build objected to.
        cause: BuildError,
    },
    /// The listener-free path failed, which leaves every `metrics!` call
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

/// The Prometheus exporter add-on. With `[engine.metrics].enabled = true` it
/// binds an HTTP listener serving `/metrics`; otherwise it installs the
/// recorder alone so `metrics::counter!` call sites stay live but no port opens.
pub struct PrometheusAddOn;

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

impl RuntimeAddOn for PrometheusAddOn {
    fn install(&self, ctx: &AddOnsContext<'_>) -> Result<AddOnHandle, BoxError> {
        if ctx.metrics.enabled {
            let addr: std::net::SocketAddr =
                ctx.metrics
                    .bind_addr
                    .parse()
                    .map_err(|cause| PrometheusError::BindAddr {
                        addr: ctx.metrics.bind_addr.clone(),
                        cause,
                    })?;
            prometheus_builder()
                .and_then(|builder| builder.with_http_listener(addr).install())
                .map_err(|cause| PrometheusError::Exporter { addr, cause })?;
            nexum_runtime_metrics::describe_all();
            info!(addr = %addr, "metrics exporter listening at /metrics");
        } else {
            // Recorder installed globally so metrics call sites stay live;
            // no HTTP port is opened. It accumulates samples in memory, unread.
            prometheus_builder()
                .and_then(|builder| builder.install_recorder().map(drop))
                .map_err(|cause| PrometheusError::Recorder { cause })?;
            nexum_runtime_metrics::describe_all();
        }
        Ok(AddOnHandle::named("prometheus"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_config::MetricsSection;
    use crate::test_utils::Refusal;

    /// The `NexumDispatchLatency` alert reads `_bucket` series by `le`, so
    /// the latency metric must render as a Prometheus histogram. The bucket
    /// list is matched on the bare name, so the call site's labels, which
    /// the dispatch path adds to, cannot cost it its bounds.
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
    #[test]
    fn prometheus_add_on_rejects_an_invalid_bind_addr() {
        let mut metrics = MetricsSection::default();
        metrics.enabled = true;
        metrics.bind_addr = "not-a-socket-addr".to_owned();
        let ctx = AddOnsContext { metrics: &metrics };
        let err = match PrometheusAddOn.install(&ctx) {
            Ok(_) => panic!("invalid bind_addr must not install"),
            Err(err) => err,
        };
        Refusal::from(crate::error::RuntimeError::AddOn(err)).variant::<PrometheusError>(
            |e| matches!(e, PrometheusError::BindAddr { addr, .. } if addr == "not-a-socket-addr"),
        );
    }
}
