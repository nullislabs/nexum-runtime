//! Pre-built guest wasm locators and the test wasmtime engine.

use std::path::PathBuf;

/// Workspace root: the topmost ancestor with a `Cargo.toml`.
pub fn workspace_root() -> PathBuf {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .filter(|d| d.join("Cargo.toml").is_file())
        .last()
        .unwrap_or(manifest)
        .to_path_buf()
}

pub fn module_wasm(module: &str) -> PathBuf {
    let artifact = module.replace('-', "_");
    workspace_root().join(format!("target/wasm32-wasip2/release/{artifact}.wasm"))
}

/// Environment opt-out for a run without the guest wasms built.
pub const ALLOW_MISSING_WASM: &str = "NEXUM_ALLOW_MISSING_WASM";

/// Built wasm for the guest package `module`.
///
/// A missing artifact fails by default, everywhere. Skipping was previously
/// the local default, which made a run without `just build` report the same
/// counts as a real one while every wasm-dependent test returned early. A
/// green suite has to mean the tests ran.
pub fn module_wasm_or_skip(module: &str) -> Option<PathBuf> {
    locate(
        module_wasm(module),
        std::env::var_os(ALLOW_MISSING_WASM).is_some(),
    )
}

pub fn example_wasm_or_skip() -> Option<PathBuf> {
    module_wasm_or_skip("example")
}

fn locate(wasm: PathBuf, allow_missing: bool) -> Option<PathBuf> {
    if wasm.exists() {
        return Some(wasm);
    }
    assert!(
        allow_missing,
        "{} not found: run `just build` first, or set {}=1 to skip every \
         wasm-dependent test. A skipped run reports the same counts as a \
         real one, so the skip is opt-in.",
        wasm.display(),
        ALLOW_MISSING_WASM,
    );
    eprintln!(
        "SKIP: {} not found and {} is set",
        wasm.display(),
        ALLOW_MISSING_WASM,
    );
    None
}

/// Test engine built from the production launch config.
pub fn test_wasmtime_engine() -> wasmtime::Engine {
    wasmtime::Engine::new(&crate::builder::wasmtime_config()).expect("wasmtime engine")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_wasm_maps_hyphens_and_appends_the_wasm_suffix() {
        let path = module_wasm("price-alert");
        assert!(
            path.ends_with("target/wasm32-wasip2/release/price_alert.wasm"),
            "unexpected artifact path: {}",
            path.display(),
        );
        assert!(path.starts_with(workspace_root()));
    }

    #[test]
    fn workspace_root_is_the_workspace_not_the_crate() {
        let manifest = std::fs::read_to_string(workspace_root().join("Cargo.toml"))
            .expect("the located root carries a Cargo.toml");
        assert!(
            manifest.contains("[workspace]"),
            "the walk stopped at a member crate rather than the workspace root",
        );
    }

    #[test]
    #[should_panic(expected = "run `just build` first")]
    fn missing_wasm_fails_by_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        locate(dir.path().join("absent.wasm"), false);
    }

    #[test]
    fn missing_wasm_skips_only_behind_the_opt_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(locate(dir.path().join("absent.wasm"), true), None);
    }

    #[test]
    fn engine_has_the_component_model_and_fuel_enabled() {
        let engine = test_wasmtime_engine();
        wasmtime::component::Component::new(&engine, "(component)")
            .expect("a trivial component compiles, so the component model is on");
        let mut store = wasmtime::Store::new(&engine, ());
        store
            .set_fuel(1)
            .expect("fuel accounting is on, so setting fuel succeeds");
    }
}
