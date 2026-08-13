//! Store and linker construction: one wasmtime `Store` per run.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use tracing::warn;
use wasmtime::component::{HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{HostMonotonicClock, HostWallClock, WasiCtxBuilder};

use super::Shared;
use super::role::Role;
use crate::bindings::EventModule;
use crate::engine_config::{ModuleLimits, OutboundHttpLimits};
use crate::host::component::{RuntimeTypes, StateHandle, StateStore};
use crate::host::extension::{Extension, HostServices, ServiceKind};
use crate::host::http::HttpGate;
use crate::host::logs::{LogSource, RunId, StdioStream};
use crate::host::state::HostState;
use crate::host_pattern::HostPattern;
use crate::manifest::ResourceSection;
use crate::module_id::ModuleId;

pub(super) type HostStore<T> = Store<HostState<T>>;

/// Shared sources let a test drive guest-visible time and the wall clock
/// extensions receive; `None` keeps the ambient clocks. `RunId.started_at`
/// is host wall-clock and unaffected.
#[derive(Clone)]
pub struct WasiClockOverride {
    pub(super) wall: Arc<dyn HostWallClock + Send + Sync>,
    pub(super) monotonic: Arc<dyn HostMonotonicClock + Send + Sync>,
}

impl WasiClockOverride {
    /// Pair the two clocks a guest can observe. Both are replaced
    /// together: a test that moves one and not the other is worse than
    /// the ambient pair.
    pub fn new(
        wall: Arc<dyn HostWallClock + Send + Sync>,
        monotonic: Arc<dyn HostMonotonicClock + Send + Sync>,
    ) -> Self {
        Self { wall, monotonic }
    }

    /// The effective host wall clock: the override's wall clock when set,
    /// else the real host clock.
    pub fn effective_wall(clocks: Option<&Self>) -> Arc<dyn HostWallClock + Send + Sync> {
        match clocks {
            Some(clocks) => clocks.wall.clone(),
            None => Arc::new(wasmtime_wasi::clocks::WallClock::default()),
        }
    }
}

struct SharedWallClock(Arc<dyn HostWallClock + Send + Sync>);

impl HostWallClock for SharedWallClock {
    fn resolution(&self) -> std::time::Duration {
        self.0.resolution()
    }

    fn now(&self) -> std::time::Duration {
        self.0.now()
    }
}

struct SharedMonotonicClock(Arc<dyn HostMonotonicClock + Send + Sync>);

impl HostMonotonicClock for SharedMonotonicClock {
    fn resolution(&self) -> u64 {
        self.0.resolution()
    }

    fn now(&self) -> u64 {
        self.0.now()
    }
}

/// `[module.resources]` layered over engine `[limits]`.
pub(super) struct ResolvedLimits {
    pub(super) fuel: u64,
    pub(super) memory: usize,
    pub(super) state_bytes: u64,
}

/// Unset `[module.resources]` fields keep the engine `[limits]` default; a
/// set field narrows and never widens.
///
/// The manifest is author-supplied, so the engine value is a ceiling rather
/// than a default. See `docs/adr/0001-operator-config-separate-and-trusted.md`.
pub(super) fn resolve_module_limits(res: &ResourceSection, cfg: &ModuleLimits) -> ResolvedLimits {
    ResolvedLimits {
        fuel: clamp("max_fuel_per_event", res.max_fuel_per_event, cfg.fuel()),
        memory: clamp("max_memory_bytes", res.max_memory_bytes, cfg.memory()),
        state_bytes: clamp("max_state_bytes", res.max_state_bytes, cfg.state_bytes()),
    }
}

/// The engine value unless the manifest asks for less. A request above the
/// ceiling is capped and logged: handing back a smaller budget than the
/// manifest declares would otherwise look like the module misbehaving.
fn clamp<T: Ord + std::fmt::Display>(field: &str, requested: Option<T>, ceiling: T) -> T {
    match requested {
        Some(value) if value > ceiling => {
            warn!(
                target: "manifest",
                field,
                requested = %value,
                ceiling = %ceiling,
                "[component.resources] exceeds the engine ceiling; using the ceiling",
            );
            ceiling
        }
        Some(value) => value,
        None => ceiling,
    }
}

/// Cached whole for restarts, so a rebuilt store is budgeted exactly like
/// the boot-time one.
pub(super) struct StoreSpec {
    pub(super) http_allowlist: Vec<HostPattern>,
    pub(super) http_limits: OutboundHttpLimits,
    /// Operator-permitted addresses that would otherwise be refused.
    pub(super) http_permitted: Vec<std::net::IpAddr>,
    pub(super) memory_limit: usize,
    pub(super) fuel: u64,
    pub(super) chain_response_max_bytes: usize,
    pub(super) state_quota: u64,
}

/// Mints the run identity for `name` at `seq` and builds its store.
pub(super) fn fresh_run_store<T: RuntimeTypes>(
    shared: &Shared<T>,
    name: &ModuleId,
    seq: u64,
    spec: &StoreSpec,
    role: Role,
) -> Result<(RunId, HostStore<T>)> {
    let run = RunId::new(name.clone(), seq);
    let store = build(shared, spec, run.clone(), role)?;
    Ok((run, store))
}

/// Takes a freshly minted [`RunId`]; `role` picks the service map.
fn build<T: RuntimeTypes>(
    shared: &Shared<T>,
    spec: &StoreSpec,
    run: RunId,
    role: Role,
) -> Result<HostStore<T>> {
    // A provider store carries an empty service map: the shared map holds
    // the registry that owns this store, and carrying it here would cycle.
    let services = match role {
        Role::Module => shared.services.clone(),
        Role::Service => HostServices::default(),
    };
    let namespace: &str = run.module.as_str();
    // Stdio is captured as tagged log records, stdin stays closed; the ctx
    // grants no network, so the allowlisted wasi:http gate is the only live path.
    let router = shared.components.logs.router();
    let mut builder = WasiCtxBuilder::new();
    builder
        .stdout(StdioStream::new(
            router.clone(),
            run.clone(),
            LogSource::Stdout,
        ))
        .stderr(StdioStream::new(
            router.clone(),
            run.clone(),
            LogSource::Stderr,
        ));
    if let Some(clocks) = &shared.clocks {
        builder.wall_clock(SharedWallClock(clocks.wall.clone()));
        builder.monotonic_clock(SharedMonotonicClock(clocks.monotonic.clone()));
    }
    let wasi = builder.build();
    let limits = wasmtime::StoreLimitsBuilder::new()
        .memory_size(spec.memory_limit)
        .build();
    let module_store = shared
        .components
        .store
        .module(namespace)
        .map_err(|e| anyhow!("local-store namespace for {namespace}: {e}"))?
        .with_quota(spec.state_quota);
    let mut store = Store::new(
        &shared.engine,
        HostState {
            wasi,
            table: ResourceTable::new(),
            limits,
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            http_gate: HttpGate::new(
                namespace,
                spec.http_allowlist.clone(),
                spec.http_limits,
                spec.http_permitted.clone(),
            ),
            run,
            log_router: router,
            chain: shared.components.chain.clone(),
            chain_response_max_bytes: spec.chain_response_max_bytes,
            // Provider guests never reach this: `build_provider_linker`
            // links only `kind.link` plus WASI.
            store: module_store,
            services,
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(spec.fuel)?;
    Ok(store)
}

/// The same `extensions` slice must drive this and capability enforcement:
/// an import instantiates only if that extension's hook is linked.
pub fn build_linker<T: RuntimeTypes>(
    engine: &Engine,
    extensions: &[Arc<dyn Extension<T>>],
) -> anyhow::Result<Linker<HostState<T>>> {
    let mut linker = Linker::<HostState<T>>::new(engine);
    EventModule::add_to_linker::<HostState<T>, HasSelf<HostState<T>>>(&mut linker, |state| state)?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    // wasi:http only; the p2 call above already covers the shared
    // wasi:io/wasi:clocks interfaces.
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    for ext in extensions {
        ext.link(&mut linker)?;
    }
    Ok(linker)
}

/// Core `nexum:host` interfaces are withheld, so a provider importing one
/// fails to instantiate; extensions are never linked into providers.
pub fn build_provider_linker<T: RuntimeTypes>(
    engine: &Engine,
    kind: &dyn ServiceKind<T>,
) -> anyhow::Result<Linker<HostState<T>>> {
    let mut linker = Linker::<HostState<T>>::new(engine);
    kind.link(&mut linker)?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::clock::ManualClock;

    /// [`build`] serves the guest `clocks.wall`; the extension seam hands out
    /// that same handle, not a second clock over the same source.
    #[test]
    fn the_effective_wall_clock_is_the_handle_the_guest_store_serves() {
        let clocks = ManualClock::new().as_override();
        let served = WasiClockOverride::effective_wall(Some(&clocks));
        assert!(Arc::ptr_eq(&clocks.wall, &served));
    }

    /// Everything outside comments and literals: line and block comments
    /// and the contents of string, byte-string, raw-string, and character
    /// literals are removed. The scan below must not fire on prose or on
    /// literal text that merely names a banned builder method, and a `//`
    /// or `/*` inside a literal must not swallow the code after it.
    fn strip_comments(src: &str) -> String {
        let is_ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
        let mut out = String::with_capacity(src.len());
        let bytes = src.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i..].starts_with(b"//") {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i..].starts_with(b"/*") {
                let mut depth = 1usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        if bytes[i] == b'\n' {
                            out.push('\n');
                        }
                        i += 1;
                    }
                }
                continue;
            }
            // A literal prefix (`b"`, `r"`, `br#"`) only starts a literal
            // at an identifier boundary; `attr"` is an identifier then a
            // plain string.
            let at_boundary = i == 0 || !is_ident(bytes[i - 1]);
            // Raw and raw byte strings: no escapes, closed by `"` plus the
            // opening run of `#`.
            if at_boundary && (bytes[i] == b'r' || bytes[i..].starts_with(b"br")) {
                let mut j = i + if bytes[i] == b'r' { 1 } else { 2 };
                let mut hashes = 0usize;
                while j < bytes.len() && bytes[j] == b'#' {
                    hashes += 1;
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'"' {
                    j += 1;
                    while j < bytes.len() {
                        if bytes[j] == b'"'
                            && bytes[j + 1..].len() >= hashes
                            && bytes[j + 1..j + 1 + hashes].iter().all(|&b| b == b'#')
                        {
                            j += 1 + hashes;
                            break;
                        }
                        j += 1;
                    }
                    out.push_str("\"\"");
                    i = j;
                    continue;
                }
                // A raw identifier such as `r#try`: fall through.
            }
            // Plain and byte strings, honouring `\"` escapes.
            if bytes[i] == b'"' || (at_boundary && bytes[i..].starts_with(b"b\"")) {
                i += if bytes[i] == b'"' { 1 } else { 2 };
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                out.push_str("\"\"");
                continue;
            }
            // Character and byte-character literals; a lifetime or loop
            // label has no closing quote after one character and is kept.
            if bytes[i] == b'\'' || (at_boundary && bytes[i..].starts_with(b"b'")) {
                let start = i + if bytes[i] == b'\'' { 1 } else { 2 };
                let close = if start >= bytes.len() {
                    None
                } else if bytes[start] == b'\\' {
                    let mut j = start + 2;
                    while j < bytes.len() && bytes[j] != b'\'' && bytes[j] != b'\n' {
                        j += 1;
                    }
                    Some(j)
                } else {
                    // One UTF-8 character, then the closing quote.
                    let len = match bytes[start] {
                        0x00..=0x7F => 1,
                        0xC0..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        _ => 4,
                    };
                    Some(start + len)
                };
                if let Some(j) = close
                    && j < bytes.len()
                    && bytes[j] == b'\''
                {
                    out.push_str("''");
                    i = j + 1;
                    continue;
                }
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    /// The cases the naive precursor got wrong: a comment marker inside a
    /// literal truncated the scan, and literal text could satisfy it.
    #[test]
    fn strip_comments_is_not_confused_by_literals() {
        // A `//` in a string must not swallow the rest of the line.
        let kept = strip_comments(r#"let a = "//"; b.inherit_env();"#);
        assert!(kept.contains(".inherit_env("));
        // A `/*` in a string must not swallow the rest of the file.
        let kept = strip_comments("let p = \"https://x/*\";\nb.inherit_network();");
        assert!(kept.contains(".inherit_network("));
        // Nor in a byte string, the shape this very file contains.
        let kept = strip_comments(r##"if s.starts_with(b"/*") {} c.initial_cwd(x);"##);
        assert!(kept.contains(".initial_cwd("));
        // Nor in a raw string or a char literal.
        let kept = strip_comments("let r = r#\"//*\"#; let c = '/'; d.inherit_args();");
        assert!(kept.contains(".inherit_args("));
        // Lifetimes are not char literals; the code after them survives.
        let kept = strip_comments("fn f<'a, 'b>(x: &'a str) { x.inherit_stdio(); }");
        assert!(kept.contains(".inherit_stdio("));
        // Literal contents cannot satisfy a match.
        let kept = strip_comments(r##"let s = ".inherit_env(";"##);
        assert!(!kept.contains(".inherit_env("));
        // Comments are still removed.
        let kept = strip_comments("// .inherit_env(\n/* .inherit_env( */");
        assert!(!kept.contains("inherit_env"));
    }

    /// Scans this crate's sources, the same shape as the emitted-metric-name
    /// guard in `metrics.rs`. The host process environment holds operator
    /// secrets by design (`${VAR}` interpolation in `engine.toml`), and the
    /// ambient `wasi:cli/environment` interface is linked into every guest,
    /// so one disclosing `WasiCtxBuilder` call would hand them to untrusted
    /// module code. The behavioural proof is
    /// `harness_guest_observes_no_environment_arguments_or_stdin`; this
    /// scan only moves the refusal to the call site.
    ///
    /// A match must look like a call: the method name preceded by `.` or
    /// `::` and followed by `(`. With comments stripped, prose or a bare
    /// string naming a method never fires, and the token list below does
    /// not match its own entries.
    #[test]
    fn no_source_in_this_crate_discloses_host_env_args_stdio_fs_or_network() {
        // `initial_cwd` is banned alongside the rest: it lands in
        // `WasiCliCtx` and a guest reads it back through the same
        // `wasi:cli/environment` interface, disclosing host filesystem
        // layout. The fixture cannot observe it through std, so the scan
        // is its only guard.
        //
        // The per-stream `inherit_std{in,out,err}` methods are listed
        // because upstream defines `inherit_stdio` as exactly those three,
        // and `socket_addr_check` because upstream defines
        // `inherit_network` as `socket_addr_check` with an allow-all
        // closure; banning only the sugar would leave each primitive
        // open. `stdin` catches both `builder.stdin(...)` and a
        // `p2::stdin()` host handle passed to it.
        const BANNED: &[&str] = &[
            "inherit_env",
            "inherit_args",
            "inherit_stdio",
            "inherit_stdin",
            "inherit_stdout",
            "inherit_stderr",
            "stdin",
            "preopened_dir",
            "inherit_network",
            "socket_addr_check",
            "initial_cwd",
        ];
        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offending: Vec<String> = Vec::new();
        let mut stack = vec![src_root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read the crate source tree") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).expect("read a source file");
                let code = strip_comments(&src);
                for token in BANNED {
                    for prefix in [".", "::"] {
                        if code.contains(&format!("{prefix}{token}(")) {
                            offending.push(format!("{} calls {token}", path.display()));
                        }
                    }
                }
            }
        }
        assert!(
            offending.is_empty(),
            "a WasiCtxBuilder call would disclose host state to every guest; \
             pass per-module values through [component.config] instead: {offending:?}",
        );
    }
}
