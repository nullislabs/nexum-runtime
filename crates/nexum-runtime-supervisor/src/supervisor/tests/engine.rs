//! What the launch engine config admits: the baseline a guest toolchain
//! emits, the WASI surface the linker carries, and fuel across a bulk memory
//! operation.

use super::*;

/// Compile `text` through the production path on `engine`.
fn compile(engine: &Engine, text: &str) -> Result<wasmtime::component::Component, RuntimeError> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fixture.wasm");
    let bytes = wat::parse_str(text).expect("assemble a component");
    std::fs::write(&path, bytes).expect("write the fixture");
    read_verified_component(engine, &path, DigestPolicy::author(None))
        .map(|(component, _)| component)
}

/// What a `wasm32-wasip2` toolchain emits unasked, and fixed-width SIMD.
#[test]
fn the_launch_config_admits_the_baseline_a_guest_toolchain_emits() {
    let baseline = "(component (core module
        (memory 1)
        (table 1 funcref)
        (global i32 (i32.add (i32.const 1) (i32.const 2)))
        (func (result i32 i32) (i32.const 1) (i32.const 2))
        (func (memory.copy (i32.const 0) (i32.const 8) (i32.const 8)))
        (func (result i32) (table.grow (ref.null func) (i32.const 1)))
        (func (param i32) (result i32) (i32.extend8_s (local.get 0)))
        (func (param f32) (result i32) (i32.trunc_sat_f32_s (local.get 0)))
        (func (param v128) (result v128) (i8x16.neg (local.get 0)))))";
    compile(&test_wasmtime_engine(), baseline).expect("the baseline compiles");
}

/// The capability gate reads a `wasi:clocks/` import as ambient whatever its
/// version, so the linker is what keeps WASI 0.3 out.
#[test]
fn the_linker_carries_wasi_0_2_and_no_wasi_0_3() {
    let engine = test_wasmtime_engine();
    let linker = build_linker::<LocalTypes>(&engine, &[]).expect("linker");
    let importing = |version: &str| {
        compile(
            &engine,
            &format!(
                r#"(component (import "wasi:clocks/monotonic-clock@{version}"
                    (instance (export "now" (func (result u64))))))"#
            ),
        )
        .expect("an import alone compiles")
    };
    assert!(linker.instantiate_pre(&importing("0.2.0")).is_ok());
    assert!(linker.instantiate_pre(&importing("0.3.0")).is_err());
}

/// One export filling `len` bytes of a 1 MiB memory, then reaching a trap of
/// its own.
fn filler(len: u32) -> String {
    format!(
        r#"(component
            (core module $m
                (memory 16)
                (func (export "run")
                    (memory.fill (i32.const 0) (i32.const 1) (i32.const {len}))
                    unreachable))
            (core instance $i (instantiate $m))
            (func (export "run") (canon lift (core func $i "run"))))"#
    )
}

/// Run the `run` export of `text` on `fuel`, returning the trap and the fuel
/// left behind.
fn run_to_trap(text: &str, fuel: u64) -> (wasmtime::Trap, u64) {
    let engine = test_wasmtime_engine();
    let component = compile(&engine, text).expect("the fixture compiles");
    let mut store = wasmtime::Store::new(&engine, ());
    store.set_fuel(fuel).expect("fuel accounting is on");
    let instance = Linker::<()>::new(&engine)
        .instantiate(&mut store, &component)
        .expect("instantiate");
    let run = instance
        .get_typed_func::<(), ()>(&mut store, "run")
        .expect("the run export");
    let err = run.call(&mut store, ()).expect_err("every fixture traps");
    let trap = *err.downcast_ref::<wasmtime::Trap>().expect("a wasm trap");
    (trap, store.get_fuel().expect("fuel accounting is on"))
}

const MIB: u32 = 1 << 20;

/// wasmtime charges a bulk operation by its size once it has run. The charge
/// must then stop the guest at once: here before the `unreachable` that
/// follows the fill.
#[test]
fn a_bulk_operation_larger_than_the_budget_ends_in_out_of_fuel() {
    let (trap, left) = run_to_trap(&filler(MIB), 1_000);
    assert_eq!(trap, wasmtime::Trap::OutOfFuel);
    assert_eq!(left, 0);
}

#[test]
fn a_bulk_operation_within_the_budget_is_charged_by_its_size() {
    let budget = 4 * u64::from(MIB);
    let (trap, left) = run_to_trap(&filler(MIB), budget);
    assert_eq!(trap, wasmtime::Trap::UnreachableCodeReached);
    let spent = budget - left;
    assert!(
        (u64::from(MIB)..u64::from(MIB) + 1_000).contains(&spent),
        "one fuel a byte, and little else: {spent}",
    );
}

/// The overshoot a single operation can take is its own size, and a memory
/// cannot outgrow its ceiling: a fill past the end traps without the work.
#[test]
fn a_bulk_operation_cannot_reach_past_the_memory_it_was_given() {
    let (trap, left) = run_to_trap(&filler(MIB + 1), 1_000);
    assert_eq!(trap, wasmtime::Trap::MemoryOutOfBounds);
    assert!(
        left > 0,
        "an operation that did no work is not charged for it"
    );
}
