`CLAUDE.md` is a symlink to this file.

## What nexum-runtime is

nexum-runtime is a WASM Component Model host runtime for web3 modules, and it supervises guest components built against the `nexum:host` WIT world.
Each module gets a capability-gated view of the host: chain access over JSON-RPC, an allowlisted `wasi:http` outbound gate, a local key-value store, clocks, and structured logging.
The host enforces fuel, memory, and epoch limits per module.

The runtime is generic and venue-agnostic: it ships no venue and no domain payload, and `crates/nexum-cli` composes the core lattice and nothing else.
A downstream layer adds its own capabilities through the extension seam in `crates/nexum-world`.
A composition root declares `[extensions.<name>]` rows in an `extensions.toml`, and `synthesize` emits those rows after the core capability rows in the module's WIT world.

This repository is the leaf of the Nullis runtime stack and carries no cross-repo dependencies.
The videre and shepherd repositories build on the SDK and the runtime published here.

## Layout

- `crates/nexum-runtime` - the engine host: wasmtime embedding, supervisor, capability providers, and metrics.
- `crates/nexum-cli` - the bare `nexum` engine binary, composed over `nexum-launch` with the `CoreRuntime` preset.
- `crates/nexum-launch` - the generic launcher: CLI parsing, config loading, tracing setup, and the run loop for a preset.
- `crates/nexum-sdk` - the guest-side SDK that modules build against, host-neutral and domain-free.
- `crates/nexum-sdk-test` - in-memory host mocks and assertion helpers for module unit tests.
- `crates/nexum-module-macros` - the `#[module]` proc-macro, reached through `nexum_sdk::module`.
- `crates/nexum-tasks` - task lifecycle and graceful shutdown, and the only crate that spawns raw `tokio` tasks.
- `crates/nexum-world` - per-module WIT world synthesis, the core capability table, and the extension registry.
- `modules/example` - the minimal reference module, with balance-tracker, http-probe, and price-alert under `modules/examples/`.
- `modules/fixtures/` - adversarial fixtures: clock-reader, flaky-bomb, fuel-bomb, memory-bomb, panic-bomb, and slow-host.
- `tools/load-gen` - the load generator for soak runs.
- `wit/nexum-host` - the `nexum:host` WIT package.

## Build, test, lint

The workspace uses Rust edition 2024.
The flake pins the toolchain to Rust 1.94.0, which matches the toolchain CI installs.
Run `nix develop` to enter the dev shell, or run `direnv allow` once.
The dev shell supplies the toolchain, the `wasm32-wasip2` target, `cargo-nextest`, `just`, `ripgrep`, and `ast-grep`.

Use the justfile recipes:

```sh
just build   # the engine plus every guest wasm
just test    # host engine unit tests through nextest
just fmt     # cargo fmt --all
just lint    # cargo clippy --workspace --all-targets --all-features -- -D warnings
just ci      # the full CI series: fmt, clippy, doc, wasms, nextest, doctests
```

Build the guest wasms before the suite: the end-to-end and fixture tests load them from `target/wasm32-wasip2/release`, and a missing artifact fails the test rather than skipping it.
Set `NEXUM_ALLOW_MISSING_WASM=1` to skip every wasm-dependent test instead, which is opt-in because a skipped run reports the same counts as a real one.

Run tests with `cargo nextest run`, not `cargo test`.
nextest does not run doctests, so run `cargo test --doc --workspace --all-features` as well.
Run `just fmt` and `just lint` before each commit, because CI fails on any rustfmt or clippy warning.

The hooks in `.claude/hooks/` support this loop.
`rustfmt-on-edit.sh` formats each edited `.rs` file.
`nextest-on-stop.sh` runs nextest for the crates with uncommitted `.rs` changes at the end of a turn.
Each hook runs only on a NixOS machine, and exits without work when `rustfmt`, `cargo`, or `cargo-nextest` is absent, so all stay silent outside the dev shell.

## Module layout

A module splits into two files.
`logic.rs` holds pure logic that does not depend on wit-bindgen, and its unit tests run against `nexum_sdk_test::MockHost`.
`lib.rs` holds the per-cdylib `wit_bindgen::generate!` glue, the generated host adapter, and the export dispatch.

A logic function binds only the host seams it uses, for example `H: ChainHost + LocalStoreHost`.
It does not bind the composed `Host` trait, which no capability-gated adapter can satisfy.
See `docs/adr/0015-host-trait-surface.md`.

## Decision records

`docs/adr` holds the decisions that constrain later work, including the trust boundary between the operator config and the module manifest, and the local-store durability model.
Read the relevant record before you change a seam it describes.
A record states a decision, its invariants, and the alternatives that were rejected and why.
It is not a migration log or a design discussion.

## Unattended work

`.claude/loop.md` carries the rules for an agent working an issue without a maintainer watching: what verifies a change, when to stop, which invariants encode a decision, and which work is never unattended.

## House rules

Do not use em-dashes in source, rustdoc, markdown, commit messages, or PR and issue bodies.
Use an ASCII hyphen, use a colon, or write two sentences.
`.claude/hooks/content-lint.sh` blocks an edit that adds an em-dash to a `.rs` or `.md` file.

Write each commit message as a Conventional Commit with an imperative subject.
Disclose AI assistance with an honest `AI Assistance: <tool> used for <what>` line in the commit message and in the PR body.
Never add a `Co-Authored-By: Claude Code` footer or a `Generated with Claude Code` line.

Keep one logical line per paragraph in a PR or issue body.
GitHub renders a single newline in a comment as a line break, so a sentence-per-line body wraps too early.

## Documentation

Write all documentation in ASD-STE100 Simplified Technical English.
Use short sentences, the active voice, and one idea per sentence.
In a markdown file, put each sentence on its own line and do not wrap within a sentence.
GitHub reflows the file when it displays it, and the diff then shows one changed line per changed sentence.
