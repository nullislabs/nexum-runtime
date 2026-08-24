//! Embed the runtime without the CLI: point the builder at a loaded config
//! and a [`Runtime`] preset, then launch and run until shutdown.
//!
//! Build the example module first (`just build-module`), then run
//! `cargo run -p nexum-runtime --example embed` from the repo root.
//!
//! [`Runtime`]: nexum_runtime::Runtime

use nexum_runtime::config::{EngineConfig, ModuleEntry};
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
