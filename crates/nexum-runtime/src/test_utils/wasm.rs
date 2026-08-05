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

/// Built artifact path for the guest package `module`.
pub fn module_wasm(module: &str) -> PathBuf {
    let artifact = module.replace('-', "_");
    workspace_root().join(format!("target/wasm32-wasip2/release/{artifact}.wasm"))
}

/// Built wasm for the guest package `module`; missing means skip locally,
/// panic under CI.
pub fn module_wasm_or_skip(module: &str) -> Option<PathBuf> {
    locate(module_wasm(module), std::env::var_os("CI").is_some())
}

pub fn example_wasm_or_skip() -> Option<PathBuf> {
    module_wasm_or_skip("example")
}

fn locate(wasm: PathBuf, ci: bool) -> Option<PathBuf> {
    if wasm.exists() {
        return Some(wasm);
    }
    assert!(
        !ci,
        "{} not found under CI: the test job must build the module wasms before the suite runs",
        wasm.display(),
    );
    eprintln!(
        "SKIP: {} not found - run `just build` to build the guest wasms",
        wasm.display(),
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
    fn missing_wasm_soft_skips_outside_ci() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(locate(dir.path().join("absent.wasm"), false), None);
    }

    #[test]
    #[should_panic(expected = "not found under CI")]
    fn missing_wasm_hard_fails_under_ci() {
        let dir = tempfile::tempdir().expect("tempdir");
        locate(dir.path().join("absent.wasm"), true);
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
