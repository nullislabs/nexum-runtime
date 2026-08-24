---
status: accepted
amends: 0022-cut-guest-to-guest-calling.md
---

# The required digest is the operator pin, and it is required by default

## Context

The engine reads two artifact pins.
`[component].digest` in the module manifest is the author pin.
`digest` on a `[[modules]]` entry in `engine.toml` is the operator pin.
Both are verified against the exact bytes handed to the compiler, and a mismatch on either refuses the boot before compile.

`[engine].require_component_digest` used to mandate the author pin, and it defaulted to `false`.
Both halves of that were wrong, for different reasons.

The default was wrong because a fail-open security control is a control nobody has.
Docker Content Trust is the standing counter-example: an opt-in signature check that stayed under 0.05% adoption over ten years.
The problem was never that a boolean existed, it was that the secure state was the one an operator had to go and find.

What the flag required was wrong because it compelled the untrusted party to certify itself.
[ADR-0001](0001-operator-config-separate-and-trusted.md) settles that the manifest is author-owned and untrusted input, and that a self-declared digest is evidence of intent rather than evidence of authorization.
The manifest is discovered, by default, as a sibling of the artifact.
Anyone who can rewrite the `.wasm` can rewrite the pin lying beside it.
The author pin buys anti-drift and anti-corruption, which are worth having.
Against an adversary who can write the artifact directory it raises the bar by zero.

The old shape also had a trap in it.
An operator who pinned correctly in `engine.toml`, in the file ADR-0001 declares trusted, was refused anyway for want of an author pin in the file ADR-0001 declares untrusted, and could reasonably conclude the flag was broken.

## Decision

`[engine].require_component_digest` requires the operator pin, and it defaults to `true`.

Every `[[modules]]` entry carries a `digest`.
An entry without one refuses at load, before compile, so unverified bytes never reach the compiler.

The author pin keeps exactly one property and loses exactly one.
It is still verified when it is present, on the same bytes, with the same refusal, reported as `DigestPin::Author`.
It is no longer the thing this flag mandates, and it never satisfies the flag: a matching author pin on an entry with no operator pin still refuses.
The two pins stay independent, so an entry that carries both and whose pins disagree refuses, with the operator's expectation reported first.
Every pin that is present is verified before the missing-pin refusal fires, so a mismatched author pin on an unpinned entry reports `DigestPin::Author` rather than the unpinned refusal.
The unpinned refusal prints the digest it wants pasted into trusted config, and it must never print a value that a pin already on disk contradicts.

### No second key

The existing key is inverted and redefined in place.
No `require_operator_digest` is added beside it.
One spelling means one thing to grep for, one thing to template, and no second permissive token that can later disagree with the first.
Relaxing is `require_component_digest = false`, an affirmative edit to the trusted file, which is the shape Deno settled on with `lock: false`.

### The refusal carries the digest it demands

`LoadRefusal::DigestUnpinned` reports the computed sha256 of the bytes it just read.
Before this record the warning path printed the digest and the refusal withheld it, so making the refusal the default would have deleted the only way to learn the value the refusal asks for.
The refusal now names the artifact, reports its digest, and names the key and file the value goes in.
Read the digest out of the error and paste it.
This works for every consumer, embedded or not, with no CLI in the loop, and `DigestMismatch` already reported `actual`, so it is consistency rather than novelty.

`nexum digest <artifact>` prints the same value ahead of time, for an operator writing the config before the first boot.

### The single-wasm override path is exempt by construction

The command-line override, `<bin> <wasm-path> [<manifest-path>]`, boots a component that no `[[modules]]` entry describes.
A requirement phrased in terms of `[[modules]]` entries has nothing to bind to there: there is no entry to carry a pin.
The override is therefore exempt, and `crates/nexum-runtime/src/builder.rs` states that at the call site by passing `require_component_digest: false` into the boot environment it synthesizes.
The refusal text names the exemption too, so it reads as a stated seam rather than an emergent one.

This is not a hole in the requirement.
The operator typed the artifact path on the command line, which is the same act of authorization the pin records in a file.
What the pin adds over the command line is durability across restarts and templating, and a config file is the thing that gets copied from staging to production.

### An embedder relaxes in Rust

An embedder constructs `EngineConfig` directly, so it can set `require_component_digest` programmatically, and `ModuleEntry.digest` is a public field, so `ModuleEntry::new(...)` followed by an assignment pins an entry from outside the crate.
Host Rust is trusted in both products this runtime serves, so there is nothing to defend at that seam.
The escape-hatch problem is config files, because config files get templated, inherited, and copied between environments.

## Consequences

- A fresh clone still runs.
  `engine.dev.toml` is committed, `just run` points at it, and it relaxes the requirement explicitly.
  The dev path no longer depends on the absence of a config file, and the first `engine.toml` an operator reads is one this repository wrote.
- Three sources reach the default and none consults another: the hand-written `Default` impl, the section-level `serde(default)` on the `engine` field, and the field-level `serde(default)` on the key.
  The third is the common operator config, an `engine.toml` with an `[engine]` table that omits the key.
  A change that edits only the impl hardens the no-config development path and leaves every real deployment fail-open, so both the impl and the named default function carry the value and one test covers all three paths.
- `BootScenario::require_digest` stays off by default and the supervisor scenario suite stays insulated.
  The scenario overwrites the engine flag from its own value, so flipping its default would silently rewrite the expected refusal of every unpinned scenario test.
- The author pin loses its only mandate, so nothing in the tree forces a manifest to carry one.
  Fourteen `component.toml` files ship and one carries a digest, the byte-stable `.wat` fixture.
  That stays true.
- An operator upgrading from an older engine hits this as the largest newly-hit refusal, and the migration notes carry it.

## Rejected alternatives

- **Default the flag `true` and leave it mandating the author pin.**
  Hygiene sold as security: it compels the untrusted party to self-certify, and the attacker who rewrites the artifact rewrites the pin beside it in the same step.
- **Add a second key for the operator pin and leave the first alone.**
  Two booleans over one subject need a precedence rule, and every precedence rule is a place where the permissive one wins by accident.
  Pre-release, redefining the one key costs a migration note and nothing else.
- **Require the operator pin and drop author-pin verification.**
  A present pin that is not checked is worse than no pin, because it reads as a control.
  Verification is free once the bytes are hashed, and the disagreement case is a real signal: a torn deployment where the operator and the author authorized different builds.
- **Pin the quickstart artifact instead of shipping `engine.dev.toml`.**
  The build is reproducible enough for it: the example wasm hashed identically when rebuilt into a fresh target directory and from a different source checkout path.
  The churn is what kills it.
  Roughly one pull request in three touches the SDK, the macros, `nexum-world`, `wit/`, `modules/example`, or `Cargo.lock`, and each of those would have to regenerate a committed pin.
- **Exempt any entry whose artifact the operator can be shown to have named.**
  The command line is the only such channel, and it is already the override path.
  Anything broader is a heuristic about operator intent, and the pin exists because intent has to be written down.
- **Widen the compile-site guard so no path can reach the compiler unpinned.**
  That defends the compile path rather than the default, and it is separate work.
