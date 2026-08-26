//! Generic engine launcher: parse the shared CLI, load the engine config,
//! initialize tracing, and drive a [`Runtime`] preset until shutdown.
//!
//! A binary is one line: `nexum_launch::run("nexum", CoreRuntime)`. The
//! preset supplies the lattice, backends, extension list, and add-ons;
//! this crate knows nothing beyond the runtime seam.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![forbid(unsafe_code)]

mod cli;
mod digest;

pub use cli::{Cli, Command};

use std::path::PathBuf;

use nexum_runtime::config::{self, EngineConfig};
use nexum_runtime::error::RuntimeError;
use nexum_runtime::{Runtime, RuntimeBuilder};
use thiserror::Error;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt;

/// Why [`run`] stopped: a subcommand's own failure, or the boot path's.
#[derive(Debug, Error)]
pub enum RunError {
    /// The boot path refused.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    /// `digest` could not read the artifact or write its line.
    #[error("cannot digest {}", path.display())]
    Digest {
        /// The artifact path as given on the command line.
        path: PathBuf,
        /// The io failure.
        #[source]
        source: std::io::Error,
    },
}

/// Parse the process arguments as `name`, then answer a subcommand or
/// [`launch`] the preset.
///
/// A subcommand short-circuits here, ahead of the config load, so
/// [`launch`] keeps taking a [`Cli`] that always means launch.
pub async fn run<R: Runtime>(name: &'static str, preset: R) -> Result<(), RunError> {
    let cli = Cli::parse_as(name);
    // Exhaustive over `Command`, so a later subcommand cannot fall through
    // to a boot the operator did not ask for.
    if let Some(command) = &cli.command {
        return match command {
            Command::Digest { path } => {
                digest::write_digest(path, &mut std::io::stdout()).map_err(|source| {
                    RunError::Digest {
                        path: path.clone(),
                        source,
                    }
                })
            }
        };
    }
    Ok(launch(name, preset, cli).await?)
}

/// Load the config, initialize tracing, and run the preset until shutdown.
pub async fn launch<R: Runtime>(name: &str, preset: R, cli: Cli) -> Result<(), RuntimeError> {
    let mut engine_cfg = config::load_or_default(cli.engine_config.as_deref())?;
    if let Some(n) = cli.log_backfill_concurrency {
        engine_cfg.engine.log_backfill_concurrency = n;
    }

    init_tracing(cli.pretty_logs, &engine_cfg);

    info!("{name} starting");

    RuntimeBuilder::new(&engine_cfg)
        .with_runtime(preset)
        .with_module_source(cli.wasm, cli.manifest)
        .launch()
        .await?
        .wait()
        .await
}

/// Install the global tracing subscriber: JSON by default, the
/// human-readable formatter behind `--pretty-logs`. The same
/// [`EnvFilter`] (`RUST_LOG`, else the config level) applies to both.
fn init_tracing(pretty: bool, engine_cfg: &EngineConfig) {
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&engine_cfg.engine.log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    if pretty {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_target(true)
            .init();
    } else {
        json_subscriber(env_filter, std::io::stdout).init();
    }
}

/// The JSON line shape `docs/production.md` publishes: event fields flattened
/// onto the object, plus a `span` object for the innermost span alone. The
/// ancestor list stays off, so a nested span hides an ancestor's fields.
fn json_subscriber<W>(env_filter: EnvFilter, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .json()
        .flatten_event(true)
        .with_span_list(false)
        .with_writer(writer)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    use nexum_runtime_testing::{JsonLogs, JsonValue, json_collector};

    /// One span, one event: enough shape for either subscriber to render.
    fn emit() {
        let span = tracing::info_span!("dispatch", module = "twap-monitor");
        let _entered = span.enter();
        info!(chain_id = 1, "dispatch ok");
    }

    fn render(f: impl Fn()) -> JsonValue {
        let sink = JsonLogs::default();
        let subscriber = json_subscriber(EnvFilter::new("info"), sink.clone());
        tracing::subscriber::with_default(subscriber, &f);
        sink.line("")
    }

    fn without_timestamp(line: JsonValue) -> JsonValue {
        let mut line = line;
        line.as_object_mut()
            .expect("a line is an object")
            .remove("timestamp");
        line
    }

    #[test]
    fn the_shared_collector_still_renders_what_the_launcher_ships() {
        let mirror = JsonLogs::default();
        tracing::subscriber::with_default(
            json_collector(mirror.clone(), tracing::Level::INFO),
            emit,
        );
        assert_eq!(
            without_timestamp(mirror.line("")),
            without_timestamp(render(emit)),
        );
    }

    #[test]
    fn a_json_line_renders_the_enclosing_span_and_its_fields() {
        let line = render(emit);
        assert_eq!(line["span"]["module"], "twap-monitor");
        assert_eq!(line["span"]["name"], "dispatch");
        assert_eq!(line["chain_id"], 1);
        assert_eq!(line["message"], "dispatch ok");
    }

    #[test]
    fn a_json_line_carries_no_ancestor_span_list() {
        let line = render(|| {
            let span = tracing::info_span!("source", source_kind = "block");
            let _entered = span.enter();
            info!("block source open");
        });
        assert!(
            line.get("spans").is_none(),
            "the ancestor list stays off: {line}",
        );
    }

    #[test]
    fn a_spanless_json_line_carries_no_span_key() {
        let line = render(|| info!("nexum starting"));
        assert!(line.get("span").is_none(), "no span, no key: {line}");
    }
}
