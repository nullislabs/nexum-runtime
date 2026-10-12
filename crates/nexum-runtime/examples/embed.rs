//! Embed the runtime without the CLI: point the builder at a loaded config
//! and a [`Runtime`] preset, then launch and run until shutdown.
//!
//! Build the example module first (`just build-module`), then run
//! `SEPOLIA_RPC_URL=<url> cargo run -p nexum-runtime --example embed` from
//! the repo root.
//!
//! [`Runtime`]: nexum_runtime::Runtime

use alloy_chains::Chain;
use anyhow::Context as _;
use nexum_runtime::config::{ChainConfig, EngineConfig, ModuleEntry, RpcEndpoint};
use nexum_runtime::{CoreRuntime, RuntimeBuilder};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The embedder owns the tracing subscriber; the library never
    // installs one.
    tracing_subscriber::fmt().init();

    let mut cfg = EngineConfig::default();
    let mut entry = ModuleEntry::new("example", "target/wasm32-wasip2/release/example.wasm");
    entry.manifest = Some("modules/example/component.toml".into());
    // This loads whatever the last `just build-module` produced, so it has no
    // stable value to pin. A deployment sets `entry.digest` from `nexum digest`
    // and leaves the requirement on.
    cfg.engine.require_component_digest = false;
    cfg.modules.push(entry);

    // The example manifest declares a block trigger on Sepolia, and chain
    // access is an operator grant, so the config names the chain. This is
    // what a `[chains.11155111]` entry in `engine.toml` resolves to, with
    // the two limits at their defaults.
    let rpc_url = std::env::var("SEPOLIA_RPC_URL")
        .context("set SEPOLIA_RPC_URL to a Sepolia JSON-RPC endpoint")?;
    cfg.chains.insert(
        Chain::sepolia(),
        ChainConfig {
            rpc_url: RpcEndpoint::try_from(rpc_url)?,
            request_timeout_secs: 30,
            max_log_range_blocks: 1000,
        },
    );

    // Bind the default preset and launch: the component builders open the
    // backends, the add-ons install, and the event loop runs until shutdown.
    let handle = RuntimeBuilder::new(&cfg)
        .runtime::<CoreRuntime>()
        .launch()
        .await?;

    // The operator surface: the handle's log pipeline serves the run/log
    // read side while (and after) the runtime runs.
    let logs = handle.logs().clone();
    handle.wait().await?;

    for meta in logs.list_runs("example") {
        let page = logs.read(&meta.run, 0);
        println!(
            "run {:?} retained {} record(s)",
            meta.run,
            page.records.len()
        );
    }
    Ok(())
}
