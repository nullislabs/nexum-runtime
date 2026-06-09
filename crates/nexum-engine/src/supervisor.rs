//! Multi-module supervisor.
//!
//! Loads every `[[modules]]` entry from `engine.toml`, instantiates
//! each as a `Shepherd` bindings against a dedicated wasmtime
//! `Store`, and routes the event types declared in each manifest's
//! `[[subscription]]` table.
//!
//! Trap handling (BLEU-817): a wasmtime trap in `on_event` marks the
//! module as `alive = false` and removes it from all future dispatch.
//! The module's subscriptions remain registered (the event-loop
//! streams are not closed) but the dispatcher skips dead modules.
//! Full restart-with-backoff lands in 0.3.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Error, Result, anyhow};
use tracing::{error, info, warn};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;

use crate::engine_config::{EngineConfig, ModuleEntry};
use crate::host::cow_orderbook::OrderBookPool;
use crate::host::local_store_redb::LocalStore;
use crate::host::provider_pool::ProviderPool;
use crate::manifest::{self, LoadedManifest, Subscription};
use crate::{HostState, Shepherd};

/// Owns every loaded module and exposes the dispatch surface the
/// event loop needs.
pub struct Supervisor {
    modules: Vec<LoadedModule>,
}

struct LoadedModule {
    name: String,
    bindings: Shepherd,
    store: Store<HostState>,
    /// Subscriptions copied from `module.toml`. The supervisor reads
    /// these on every event to decide whether to dispatch.
    subscriptions: Vec<Subscription>,
    /// Set to `false` when `on_event` traps. Dead modules are silently
    /// skipped on every subsequent dispatch. Full restart-with-backoff
    /// lands in 0.3.
    alive: bool,
}

impl Supervisor {
    /// Compile + instantiate every module declared in
    /// `engine_cfg.modules`. The wasmtime `Engine` + `Linker` are
    /// passed in so `main.rs` can build them once (the bindgen
    /// `Shepherd::add_to_linker` call binds them to `HostState`,
    /// which the supervisor does not re-derive).
    pub async fn boot(
        engine: &Engine,
        linker: &Linker<HostState>,
        engine_cfg: &EngineConfig,
        cow_pool: &OrderBookPool,
        provider_pool: &ProviderPool,
        local_store: &LocalStore,
    ) -> Result<Self> {
        let mut modules = Vec::with_capacity(engine_cfg.modules.len());
        for entry in &engine_cfg.modules {
            let loaded =
                Self::load_one(engine, linker, entry, cow_pool, provider_pool, local_store)
                    .await
                    .with_context(|| format!("load module {}", entry.path.display()))?;
            modules.push(loaded);
        }
        info!(count = modules.len(), "supervisor up");
        Ok(Self { modules })
    }

    /// One-shot construction from a single ad-hoc `(component, manifest)`
    /// pair. Used by the CLI-positional invocation so `just run`
    /// against the example module keeps working without an
    /// `engine.toml`.
    pub async fn boot_single(
        engine: &Engine,
        linker: &Linker<HostState>,
        wasm: &Path,
        manifest: Option<&Path>,
        cow_pool: &OrderBookPool,
        provider_pool: &ProviderPool,
        local_store: &LocalStore,
    ) -> Result<Self> {
        let entry = ModuleEntry {
            path: wasm.to_path_buf(),
            manifest: manifest.map(Path::to_path_buf),
        };
        let loaded =
            Self::load_one(engine, linker, &entry, cow_pool, provider_pool, local_store).await?;
        Ok(Self {
            modules: vec![loaded],
        })
    }

    async fn load_one(
        engine: &Engine,
        linker: &Linker<HostState>,
        entry: &ModuleEntry,
        cow_pool: &OrderBookPool,
        provider_pool: &ProviderPool,
        local_store: &LocalStore,
    ) -> Result<LoadedModule> {
        // Canonical name is module.toml (ADR-0001). nexum.toml is accepted
        // with a deprecation warning during the 0.1→0.2 transition.
        let manifest_path = entry.manifest.clone().or_else(|| {
            let dir = entry.path.parent()?.to_owned();
            let canonical = dir.join("module.toml");
            if canonical.exists() {
                return Some(canonical);
            }
            let legacy = dir.join("nexum.toml");
            if legacy.exists() {
                eprintln!(
                    "[deprecation] nexum.toml is deprecated; rename to module.toml \
                     (ADR-0001). Support will be removed in 0.3."
                );
                return Some(legacy);
            }
            None
        });
        let loaded_manifest: LoadedManifest = match manifest_path.as_deref() {
            Some(p) if p.exists() => {
                info!(manifest = %p.display(), "loading module manifest");
                manifest::load(p)?
            }
            _ => {
                warn!(
                    component = %entry.path.display(),
                    "no module.toml — falling back to anonymous module"
                );
                manifest::fallback_manifest()
            }
        };

        // Compile + instantiate.
        info!(component = %entry.path.display(), "compiling component");
        let component = Component::from_file(engine, &entry.path)
            .map_err(Error::from)
            .with_context(|| format!("compile {}", entry.path.display()))?;

        // Enforce capability declarations before spending time on instantiation.
        manifest::enforce_capabilities(
            &loaded_manifest,
            component.component_type().imports(engine).map(|(n, _)| n),
        )
        .map_err(|e| Error::msg(e.to_string()))
        .with_context(|| format!("capability violation in {}", entry.path.display()))?;
        let wasi = WasiCtxBuilder::new().inherit_stdio().build();
        let module_namespace = if loaded_manifest.manifest.module.name.is_empty() {
            "module".to_owned()
        } else {
            loaded_manifest.manifest.module.name.clone()
        };
        let mut store = Store::new(
            engine,
            HostState {
                wasi,
                table: ResourceTable::new(),
                monotonic_baseline: std::time::Instant::now(),
                http_allowlist: loaded_manifest.http_allowlist.clone(),
                module_namespace: module_namespace.clone(),
                cow: cow_pool.clone(),
                chain: provider_pool.clone(),
                store: local_store.clone(),
            },
        );
        let bindings = Shepherd::instantiate_async(&mut store, &component, linker)
            .await
            .map_err(Error::from)
            .with_context(|| format!("instantiate {}", entry.path.display()))?;

        // Call `init` with the manifest's `[config]`.
        let config: crate::Config = if loaded_manifest.config.is_empty() {
            vec![("name".into(), module_namespace.clone())]
        } else {
            loaded_manifest.config.clone()
        };
        match bindings
            .call_init(&mut store, &config)
            .await
            .map_err(Error::from)?
        {
            Ok(()) => info!(module = %module_namespace, "init succeeded"),
            Err(e) => warn!(
                module = %module_namespace,
                domain = %e.domain,
                kind = ?e.kind,
                code = e.code,
                message = %e.message,
                "init failed",
            ),
        }

        // Surface any `[[subscription]]` entries the host cannot
        // service yet, so an operator running 0.2 against a 0.3
        // manifest does not silently drop events.
        for sub in &loaded_manifest.manifest.subscriptions {
            if matches!(sub, Subscription::Cron { .. }) {
                warn!(
                    module = %module_namespace,
                    "cron subscriptions are declared but inert in 0.2 (lands in 0.3)",
                );
            }
        }

        Ok(LoadedModule {
            name: module_namespace,
            bindings,
            store,
            subscriptions: loaded_manifest.manifest.subscriptions.clone(),
            alive: true,
        })
    }

    /// Number of modules currently loaded.
    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    /// Set of chain ids any module asked for block events on. The
    /// caller opens one shared block subscription per chain id and
    /// routes through `dispatch_block`.
    pub fn block_chains(&self) -> BTreeSet<u64> {
        let mut out = BTreeSet::new();
        for module in &self.modules {
            for sub in &module.subscriptions {
                if let Subscription::Block { chain_id } = sub {
                    out.insert(*chain_id);
                }
            }
        }
        out
    }

    /// Per-module log subscriptions. Each entry is a `(module_name,
    /// chain_id, filter)` triple the event loop opens against the
    /// matching alloy provider; the resulting stream tags every log
    /// with `module_name` so `dispatch_log` routes correctly.
    pub fn log_subscriptions(&self) -> Vec<(String, u64, alloy_rpc_types_eth::Filter)> {
        let mut out = Vec::new();
        for module in &self.modules {
            for sub in &module.subscriptions {
                if let Subscription::Log {
                    chain_id,
                    address,
                    event_signature,
                } = sub
                {
                    match build_alloy_filter(address.as_deref(), event_signature.as_deref()) {
                        Ok(filter) => out.push((module.name.clone(), *chain_id, filter)),
                        Err(err) => warn!(
                            module = %module.name,
                            chain_id,
                            error = %err,
                            "invalid log subscription — skipping",
                        ),
                    }
                }
            }
        }
        out
    }

    /// Dispatch a block event to every module subscribed to
    /// `block.chain_id`. Returns the number of modules invoked.
    /// Modules that trap are marked dead and excluded from future dispatch.
    pub async fn dispatch_block(&mut self, block: crate::nexum::host::types::Block) -> usize {
        let event = crate::nexum::host::types::Event::Block(block);
        let chain_id = match &event {
            crate::nexum::host::types::Event::Block(b) => b.chain_id,
            _ => unreachable!(),
        };
        let mut dispatched = 0;
        for module in &mut self.modules {
            if !module.alive {
                continue;
            }
            let subscribed = module
                .subscriptions
                .iter()
                .any(|s| matches!(s, Subscription::Block { chain_id: cid } if *cid == chain_id));
            if !subscribed {
                continue;
            }
            match module
                .bindings
                .call_on_event(&mut module.store, &event)
                .await
            {
                Ok(Ok(())) => dispatched += 1,
                Ok(Err(host_err)) => warn!(
                    module = %module.name,
                    chain_id,
                    domain = %host_err.domain,
                    kind = ?host_err.kind,
                    message = %host_err.message,
                    "on-event returned host-error",
                ),
                Err(trap) => {
                    error!(
                        module = %module.name,
                        chain_id,
                        error = %trap,
                        "on-event trapped — module marked dead, removed from dispatch",
                    );
                    module.alive = false;
                }
            }
        }
        dispatched
    }

    /// Dispatch a log event to the specific module that opened the
    /// subscription. Returns `true` when the module accepted the dispatch;
    /// `false` when the module is dead, not found, or its callback failed.
    /// A trapping module is marked dead and excluded from future dispatch.
    pub async fn dispatch_log(
        &mut self,
        module_name: &str,
        chain_id: u64,
        log: alloy_rpc_types_eth::Log,
    ) -> bool {
        let target = match self.modules.iter_mut().find(|m| m.name == module_name) {
            Some(m) => m,
            None => {
                warn!(module = %module_name, "no such module — dropping log");
                return false;
            }
        };
        if !target.alive {
            return false;
        }
        let event = crate::nexum::host::types::Event::Logs(vec![project_log(chain_id, &log)]);
        match target
            .bindings
            .call_on_event(&mut target.store, &event)
            .await
        {
            Ok(Ok(())) => true,
            Ok(Err(host_err)) => {
                warn!(
                    module = %module_name,
                    chain_id,
                    domain = %host_err.domain,
                    kind = ?host_err.kind,
                    message = %host_err.message,
                    "on-event returned host-error",
                );
                false
            }
            Err(trap) => {
                error!(
                    module = %module_name,
                    chain_id,
                    error = %trap,
                    "on-event trapped — module marked dead, removed from dispatch",
                );
                target.alive = false;
                false
            }
        }
    }

    /// Count of modules currently alive (not dead due to traps).
    pub fn alive_count(&self) -> usize {
        self.modules.iter().filter(|m| m.alive).count()
    }
}

/// Project an alloy `Log` onto the WIT `log` record. The chain id
/// is not on the alloy log (the subscription context carries it),
/// so we receive it alongside.
fn project_log(chain_id: u64, log: &alloy_rpc_types_eth::Log) -> crate::nexum::host::types::Log {
    crate::nexum::host::types::Log {
        chain_id,
        address: log.address().as_slice().to_vec(),
        topics: log.topics().iter().map(|t| t.as_slice().to_vec()).collect(),
        data: log.inner.data.data.to_vec(),
        block_number: log.block_number.unwrap_or(0),
        transaction_hash: log
            .transaction_hash
            .map(|h| h.as_slice().to_vec())
            .unwrap_or_default(),
        log_index: log.log_index.unwrap_or(0) as u32,
    }
}

/// Translate a `[[subscription]]` log entry into an alloy `Filter`.
fn build_alloy_filter(
    address: Option<&str>,
    event_signature: Option<&str>,
) -> Result<alloy_rpc_types_eth::Filter> {
    use alloy_primitives::{Address, B256};
    let mut filter = alloy_rpc_types_eth::Filter::new();
    if let Some(addr_hex) = address {
        let addr: Address = addr_hex
            .parse()
            .map_err(|e| anyhow!("invalid log address {addr_hex:?}: {e}"))?;
        filter = filter.address(addr);
    }
    if let Some(topic_hex) = event_signature {
        let topic: B256 = topic_hex
            .parse()
            .map_err(|e| anyhow!("invalid topic {topic_hex:?}: {e}"))?;
        filter = filter.event_signature(topic);
    }
    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_supervisor_returns_no_subscriptions() {
        let sup = Supervisor {
            modules: Vec::new(),
        };
        assert!(sup.block_chains().is_empty());
        assert!(sup.log_subscriptions().is_empty());
        assert_eq!(sup.module_count(), 0);
    }
}
