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
use tracing::{Metadata, info};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{FilterFn, filter_fn};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

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

/// The log target of the HTTP client under the chain providers. It prints
/// the address it dials at DEBUG and the host name at TRACE, so its lines
/// would name every RPC endpoint.
const HTTP_CLIENT_TARGET: &str = "hyper_util::client";

/// Drops every line from [`HTTP_CLIENT_TARGET`]. A filter of its own and
/// not an [`EnvFilter`] directive, which a longer `RUST_LOG` target would
/// override.
fn without_dial_addresses() -> FilterFn<impl Fn(&Metadata<'_>) -> bool> {
    filter_fn(|meta| !meta.target().starts_with(HTTP_CLIENT_TARGET))
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
            .finish()
            .with(without_dial_addresses())
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_target(true)
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .finish()
            .with(without_dial_addresses())
            .init();
    }
}

#[cfg(test)]
mod tests {
    use nexum_runtime_logs::LogCapture;

    use super::*;

    /// What the filter admits of one line from the HTTP client and one from
    /// the RPC client beside it.
    fn admitted(directives: &str) -> String {
        let capture = LogCapture::new();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(directives))
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish()
            .with(without_dial_addresses());
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(
                target: "hyper_util::client::legacy::connect::http",
                "connecting to 127.0.0.1:8545"
            );
            tracing::trace!(target: "hyper_util::client::legacy::pool", "checkout");
            tracing::trace!(target: "alloy_rpc_client::call", "sending request");
        });
        capture.text()
    }

    #[test]
    fn the_http_client_target_is_off_at_every_level() {
        // The second filter is a `RUST_LOG` that asks for the target by name.
        for directives in [
            "trace",
            "info,hyper_util::client::legacy=trace,alloy_rpc_client=trace",
        ] {
            let out = admitted(directives);
            assert!(!out.contains("hyper_util"), "{directives}: {out}");
            assert!(out.contains("sending request"), "{directives}: {out}");
        }
    }
}
