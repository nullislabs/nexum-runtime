---
status: superseded by ADR-0017; historical
---

# Capabilities and services: one model, four names

> Historical: [ADR-0017](../adr/0017-capabilities-and-services.md) records the decision this document argued for.
> `HostService`, `ServiceKind`, `Extension::service`, `SERVICE_CAPABILITIES`, `PROVIDER_CAPABILITIES`, and `Role::Adapter` below no longer occur in `crates`.
> Read this document for the reasoning, not for the current names.

A **host capability** is a WIT interface the runtime implements in Rust, named in a namespace the runtime owns, bound into a component's world at build time, and bounded by an operator dial.
An **extension capability** is the same thing with the Rust written by a composition root behind the `Extension` seam, registered at boot instead of compiled into the `CORE` table.
A **service** is a guest component that exports a versioned WIT interface another guest component imports, authorized by the operator, called across a store boundary.

There is a fourth thing, and it is a composite, not a peer.
An **extension backend** is a guest component installed by an extension as the implementation of that extension's own capability.
It is concept 3 used as the substrate for concept 2.
Today it is called `HostService`, `ServiceKind` and `[[services]]`, which is why the vocabulary collapsed.

This document is the corrected model, the migration, and the objections that were raised against it.
Two objections were fatal and both are fixed here.
Twenty-two more changed the plan.
Five stand unresolved and are listed at the end.

## The concepts

| Concept | Definition | Implemented by | Declared by | Authorized by |
| --- | --- | --- | --- | --- |
| Host capability | WIT interface under `nexum:host/`, plus the transport gates | The runtime, in Rust | Author, in `[dependencies]`, by capability name | Operator, `[policy].capabilities` and per-component rows; request intersected with policy, shortfall refuses at boot |
| Extension capability | WIT interface under an extension's own package | A composition root, through `Extension::link` | Author, in `[dependencies]`, by capability name | Same as a host capability; the operator also chooses which extensions the binary carries |
| Service | Versioned WIT interface exported by a guest component | A third-party guest component | Provider: `provides`. Consumer: `[dependencies]`, by interface id | Operator, `[implements]`, binding an interface track to an operator-written component id, with a digest pin |
| Extension backend | A guest component implementing an extension's capability | A third-party guest component, wired by extension Rust | Author: `kind = "backend"` plus `provides` | Operator, `[[backends]]` plus the same `[implements]` table |

Two rules cover the whole table.

For a capability the operator **restricts** runtime code that exists whether or not a guest wants it.
For a service the operator **selects** among untrusted third-party artifacts.
Restriction and selection are different acts, so they never share a config shape.

## Concept 1: host capability

**Today.**
`nexum-world` holds `Cap`, `CORE`, `WasiCap` and `WASI_GATES`.
`Cap` mixes five import-bearing `nexum:host` rows with `Http`, which emits no world import and is a pure transport gate.
That mix forces `Capability.import` and `Capability.adapter` to be `Option`, forces a hand-written invariant test at `nexum-world/src/lib.rs:897`, and forces `WASI_GATES` to splice `Cap::Http` in front of the `WasiCap` names.
`manifest/capabilities.rs` then names the service-world subset `PROVIDER_CAPABILITIES`, which PR #224 proposes to rename to `SERVICE_CAPABILITIES`.

**Becomes.**
Two enums, because there are two things.

- `HostCap`: import-bearing rows only, with `import` and `adapter` non-optional.
- `Gate`: `http`, `wasi-sockets`, `wasi-filesystem`.
  A gate names a linked host surface that carries no synthesized import.

This does **not** wait on #39.
The earlier draft made #39 a prerequisite and that was wrong: giving `Cap::Http` a real world import means emitting `import wasi:http/outgoing-handler`, and `resolve_wit_packages` refuses any package not vendored in the nearest ancestor `wit/` tree.
The repository has no `wit/deps`, guests reach HTTP through `wstd` on `wasm32-wasip2`, and the macro would emit a second independent binding of the same interfaces into every guest crate.
It would also break `core_table_carries_no_extension_row`, which asserts every `CORE` import starts with `nexum:host/` and every core row has empty `packages`, and it would force `NamespaceCaps` to grow a per-row prefix.
The gate is a real category, not a defect.
#39 keeps its second branch: document the gap.

Collapse the four spellings of the core set to two: `CORE` for the rows, `CORE_CAPABILITIES` for the names.
Replace `Self::VARIANTS[self as usize]` with a hand-written `const fn as_str` match.

## Concept 2: extension capability

**Today.**
An extension registers a namespace through `Extension::namespace`, links host Rust through `Extension::link`, and publishes host-side state through `Extension::service` as `Arc<dyn HostService>`.
`synthesize` refuses an extension name that collides with a core name.
`CapabilityRegistry::register` refuses nothing.
The build-time and boot-time registries disagree about what is legal.

**Becomes.**
`ExtCapName`, a newtype with a private field, whose smart constructor owns both checks: core collision and duplicate registration.
It keeps a `&'static`-borrowing form so composition roots keep const `NamespaceCaps` tables.

Renames, so that concept 2 loses the word `service` completely:

| Today | Becomes |
| --- | --- |
| `Extension::namespace()` | `Extension::name()` |
| `Extension::service()` | `Extension::backend_state()` |
| `Extension::provider()` | `Extension::backend_installer()` |
| `HostService` | `ExtensionBackendState` |
| `HostServices` | `ExtensionBackends` |
| `HostState::services` | `HostState::backends` |
| `ServiceKind` | `BackendInstaller` |
| `ServiceKinds` | `BackendInstallers` |
| `DuplicateServiceNamespace` | `DuplicateExtensionName` |

`Extension::provider()` must **not** become `service_kind()`, which is what PR #224 currently does.
That would attach concept 3's word permanently to concept 4.

## Concept 3: service

**Today.**
It does not exist.
A `[[services]]` entry is selected by `shared.kinds.get(name)` at `supervisor/load.rs`, against a `BTreeMap<&'static str, ServiceRow<T>>` built from extension Rust.
A service's type is therefore its manifest `name`, which is author-controlled and checkable against nothing.
`synthesize` emits only `world module` with fixed `init` and `on-event` exports, so no component can have a synthesized world that exports an interface.
Every `[[services]]` entry on main is an extension backend.

**Becomes.**

`InterfaceId`, a newtype over package, interface and version, distinct from `ModuleId`.
The provider claims it with `provides`; the runtime verifies the claim against `component_type().exports`.
The consumer names it as a key in `[dependencies]`.
The operator binds it in `[implements]`.

Naming and typing are separate steps, and the first draft conflated them.
An interface id is a **name** an untrusted author chooses.
Verifying `provides` against exports proves self-consistency only.
The **type** check is structural: at boot, compare the consumer's imported instance type against the provider's exported instance type with `wasmtime::component::types` structural equality, and refuse on mismatch.
Wasmtime's semver track in `Linker` is documented as an assumption about well-behaved hosts, and the host here is an untrusted guest, so that assumption is not available to us.
`func_new_async` is untyped, so without the structural check a divergent signature surfaces as a first-dispatch error, which breaks ADR-0001's rule that a request is granted whole or the component does not boot.

Three further constraints, each a refusal at boot:

- A provided interface may not carry a store-bound handle.
  That is `resource`, `future`, `stream` and `error-context`, not resources alone.
- The `[implements]` graph must be acyclic.
  `ActorSlot` is a non-reentrant `tokio::sync::Mutex` held across the whole call, so any A to B to A cycle parks until the dispatch deadline, on every event, permanently.
  Component composition gave acyclicity for free; we reject composition, so we buy acyclicity back with an explicit check.
- The consumer's world must import the interface at a full version, and the WIT package must be vendored.
  The package directory is derived from the interface id by replacing `:` with `-`, which is the rule `nexum:host` to `nexum-host` already follows.

The host keeps typed bindings for the lifecycle only.
A service is instantiated with `Linker::instantiate_async`, then wrapped by a small `init`-only bindgen world, then reflected dynamically for the provided interface.
Bindgen looks exports up by name, so extra exports do not break the lifecycle bindings.

## Concept 4: extension backend

**Today.**
`BackendInstaller` (`ServiceKind`) registers under a Rust `&'static str`, and `load.rs` selects it with the manifest `name`.
An author cannot write a valid manifest without reading the extension's Rust source.

**Becomes.**
It survives, renamed, and it is keyed on an `InterfaceId` like everything else.
`BackendInstaller::interface() -> InterfaceId` replaces `kind() -> &'static str`, so the identifier the author must write appears in the extension's WIT rather than only in its Rust.
The manifest spells `kind = "backend"`.
The engine file spells `[[backends]]`.

This last point is the fix for the sharpest operator-facing objection.
Renaming Rust types does nothing for the two files a human writes.
Two entries with different wiring regimes and different operator obligations must not both spell `[[services]]` with `kind = "service"`.

## Shared machinery

Shared, each justified by the component model, not by convenience.

1. **One Store per component**, with its memory ceiling, fuel budget, keccak local-store namespace, HTTP gate, run identity, and restart and poison window.
   Capability enforcement is `admit_and_verify` comparing *this* component's imports against *this* component's manifest.
   That check is only meaningful per instance.
   This is also the argument against wac composition: a composed instance graph presents the union of unsatisfied imports and hides internal edges, so a chain-less plugin becomes indistinguishable from a chain-granted module.
   Composition is not rejected on taste.
   It deletes the boundary the trust model is built on.
2. **Admission and digest verification.**
   Every kind of component is an untrusted artifact at an operator-named path, so `read_verified_component`, the extension-section claim walk, and the import enforcement walk stay one code path.
   Duplicating the security-critical path is never worth it.
3. **Lifecycle, liveness, poison, restart, and the serialized `ActorSlot`.**
4. **The `[dependencies]` table.**
   One table, one entry per dependency, and the key is always the canonical name of the thing depended on.

Not shared, and this is where the code is wrong today.

1. **Type identity.**
   A host capability has no type identity and needs none: it is a name in a namespace the runtime owns and fixes at compile time.
   A service has exactly one, and it is structural.
   Today a service's type is its manifest `name`, which violates ADR-0001's rule that policy never keys on an author-chosen field.
2. **The resolution table.**
   A capability resolves against a `&'static` const table fixed at compile time.
   A service resolves against a boot-time registry of what actually loaded, under an operator binding.
   #205's current scope sentence fuses them, and its own note that "`NamespaceCaps` holds `&'static str`, so dynamic interfaces force owned strings through the registry" is the type system reporting the category error.
   Two resolvers, not one widened type.
   Resolution never consults the other table.
   A **refusal message** may, and must, so that depending on a component by name prints the corrected line.
3. **The linker.**
   This is a change from the first draft.
   It was claimed that one linker cannot serve both, as a component-model consequence.
   That is false: both satisfiers are `Definition::Func` entries and `func_new_async` may await a call on another store.
   The real reason is enforcement scope.
   `enforce_capabilities` only refuses an import that `wit_import_to_cap` recognizes, and that function returns `None` for anything outside `wasi:` and the registered namespaces.
   So an undeclared import of `nexum:wallet/signer` passes admission today and is stopped only by the linker having no such definition.
   One shared linker plus one service trampoline turns that accident into a hole for every other component.
   Therefore: **the import walk becomes fail-closed, and the linker becomes per component**, built after admission from that component's resolved dependency set.
4. **The authorizer.** Restriction versus selection, as above.
5. **The word "service".** It belongs to concept 3 alone.

One thing shared by accident that should not be: `build_provider_linker` calls only `kind.link()`, while the namespace the manifest is validated against is a separate static list.
Nothing checks that the two agree.

One thing the first draft claimed as justified and is not: the per-component fuel budget does not survive a service call.
`SupervisedStore::call` refuels before every routed call, so N service calls inside one `on-event` cost N full provider budgets, none charged to the caller.
The same holds for the state quota and the memory ceiling.
This is a defect the trampoline work must repair, not a property the model already has.

## The author surface

`component.toml` is untrusted.
A module, a service and a backend write the same surface.

```toml
[component]
name     = "acme-wallet"                    # identity and the local-store namespace, nothing else
version  = "0.1.0"
kind     = "service"                        # required; no default
provides = "nexum:wallet/signer@2.0.0"      # required for service and backend; a claim, verified against real exports
digest   = "sha256:..."                     # evidence of intent, never of authorization

[dependencies]
chain                        = {}
logging                      = {}
http                         = { hosts = ["api.cow.fi"] }
acme-metrics                 = {}
"nexum:quotes/feed@1.3.0"    = {}

[component.resources]
max_state_bytes = 1048576                   # narrows an operator ceiling, never widens
```

The key is the canonical name of the thing depended on.
A bareword is a capability name, and it already must match the capability exactly, because `enforce_capabilities` compares the declared key set against `wit_import_to_cap` output.
A quoted interface id is a service dependency.
The discriminator is therefore the key form, which is structural and cannot shadow.

This replaces the alias grammar in the earlier draft and in #205.
The alias was justified as "what the author's own code calls", and that is true only for a capability, where the alias coincides with the generated path by construction.
For a service, wit-bindgen names the generated module from the interface id, and there is no rename mechanism, so the alias would appear in the manifest and nowhere in the author's code.
One key rule, no fiction.

Resolution order is not the discriminator.
Order-based resolution means that adding a name to the core table later silently re-points an existing author's dependency at a different thing, which is a shadowing hazard on a security-relevant grant.
This makes ADR-0016 lines 59 to 61 wrong, not merely unimplemented.

Three things a service gains that it does not have today.
It declares its own host capabilities like any other component.
Its declared `http.hosts` is honoured rather than silently discarded, which `supervisor/load.rs:495` does today by reading `entry.http_allow` and never reading `loaded_manifest.http_allowlist`.
`kind` is required, so an omitted field cannot silently demote a service to a module.

## The operator surface

`engine.toml` is trusted.

```toml
[policy]                                    # what any component may hold, and how much
max_memory_bytes      = 268435456
max_fuel_per_dispatch = 1000000000
max_state_bytes       = 1073741824
capabilities          = ["chain", "logging", "http"]
http_deny             = ["169.254.0.0/16"]

[policy.total]
max_memory_bytes = 4294967296

[policy.component.wallet]                   # keyed on the operator-written id below
max_memory_bytes = 1073741824
http_allow       = ["api.cow.fi"]

[chains.mainnet]
rpc_url = "..."

[[modules]]
id     = "tracker"
path   = "/var/nexum/balance-tracker.wasm"
digest = "sha256:..."

[[services]]                                # concept 3: operator-visible guest services
id     = "wallet"
path   = "/var/nexum/acme-wallet.wasm"
digest = "sha256:..."

[[backends]]                                # concept 4: an extension's own implementation
id     = "venue-registry"
path   = "/var/nexum/venues.wasm"
digest = "sha256:..."

[implements]                                # the sole authorization of interface wiring
"nexum:wallet/signer@2" = "wallet"
"acme:venues/registry@0" = "venue-registry"

[extensions.acme]                           # opaque to the engine, by design
```

`id` is new and it is load-bearing.
ADR-0001's closing consequence says policy that binds to a component must key on an identifier the operator writes.
The first draft quoted that rule against #152 and then bound `[implements]` to `[component].name`, which is exactly the violation.
`id` is the join column for `[policy.component]`, the value of every `[implements]` row, and the key for a targeted egress allowlist.
Neither `ModuleEntry` nor `ServiceEntry` has such a field today, so adding it is scheduled work, not an assumption.

`[implements]` keys on the compatibility **track**, not the full version.
The track is the major for versions at or above 1.0, and major.minor below it, matching #205's `version_compat_track`.
WIT text needs a full version, so `provides` and a service dependency key carry `@2.0.0`.
The authorization row names `@2`, so a provider patch bump does not force an edit to trusted config.
The digest pin, not the version, fixes the exact artifact.

`[policy]` supersedes the three scalar fields `fuel_per_event`, `memory_bytes` and `state_bytes` on `[limits]`.
The `[limits]` subsections `http`, `chain`, `logs`, `poison` and `dispatch` stay where they are.
`EngineConfig` carries `deny_unknown_fields`, so this is one breaking config change and it must be made once.

Egress: the effective host set is the author's `hosts`, intersected with `[policy.component.<id>].http_allow` where one is present, minus `[policy].http_deny`.
`[[services]].http_allow` retires into `[policy.component]`, not into a global denylist alone.
The earlier draft retired it into a global CIDR list, which would have moved a per-service hostname decision out of the trusted file and given nothing back that can express a hostname.

Absent-record defaults, which the first draft left unstated:

- Absent `[policy]`: today's `[limits]` defaults apply.
  Fail closed by capacity, not by enumeration, as #152 states.
  A component the operator never named still gets defaults and still counts against the total.
- Absent `[policy].capabilities`: every capability the runtime supports is permitted.
  Fail closed on absence would break every existing deployment on upgrade, and #127 must say so.
- Absent `[implements]` row: the implementer does not load.
  Selection has no permissive default.

`provides` is a claim, never authorization.
An author can genuinely export `nexum:wallet/signer` and log every key it sees.

## Migration plan

**1.
ADR-0017, superseding ADR-0016 in part.**
ADR-0016's grammar decisions stand: component as the unit, `kind`, `[dependencies]` as the request direction, retiring provider, adapter and worker on the guest path.
Two things are struck.
Line 90, "Naming a service and naming a host capability read the same, because the engine already treats them the same", which is the sentence that licenses the conflation and which #205 cites as authority.
Lines 59 to 61, the resolution-order rule, replaced by the canonical-key rule.
Records the four concepts, the key form, `kind` required, and the two-enum split of `Cap`.
Rewrites: ADR-0016.
Unblocks: everything below.

**2.
PR #224, amended in review, then merged.**
Subsumes #202.
The component-path half is correct and proceeds: `LoadedProvider` to `LoadedService`, `install_provider`, `fn provider` to `fn service`.
Corrected in flight: `Extension::provider()` becomes `backend_installer()`, not `service_kind()`.
Folded in, so downstream absorbs one break instead of two: the concept 2 renames in the table above, plus `admit_worker` to `admit_module`, which is a default method on the public `Extension` trait and is not the private tidy the first draft called it.
The capability-machinery constants become crate-private and take the name `BACKEND_CAPABILITIES`, `BACKEND_NAMESPACE` and `CapabilityRegistry::backend()`.
They are transitional and die at step 11, so making them private now means their death costs nothing downstream.
`tests/naming_guard.rs` gets four buckets: the alloy `Provider` collision, `service` only on concept 3, `backend` only on concept 4, and neither word on capability or world machinery.
Note that videre implements `Extension::service()` and `provider()` and `HostService` today, so #202's "zero downstream implementors" is out of date and the break is real.

**3.
Crate-private tidy.**
`Role::Service` still emits `adapter` in every tracing field and message while `Role::label()` returns `service`, so one instance appears as two things in logs and metrics.
Also `LoadRefusal::WorkerKindAdapter`, the `ServiceRow` doc, and the "One loaded provider" rustdoc.
`admit_provider` and `provider_kinds` are already in #224 and are not repeated here.

**4.
One operator config change, decided and landed once.**
Subsumes the shape half of #152.
Unblocks #121, #122, #123, #127, #128, #153.
Feeds #207.
Adds `id` to `[[modules]]` and `[[services]]`, adds `[[backends]]`, adds `[[services]].digest`, adds `[implements]`, adds `[policy]` with `total` and `component` tables, retires `[[services]].http_allow` into `[policy.component]`, and settles `[limits]`.
All of it in one breaking change, because `deny_unknown_fields` means a second change is a second hard boot failure for every operator.
The first draft scheduled the `http_allow` retirement two steps after the step that froze `[[services]]`, which is the exact failure step 4 exists to prevent.
Corrects the record: #152 already says "keyed by a name the module author cannot choose" and "the override key is an operator-written identifier", so the criticism of its example key was aimed at a position it does not hold.
Its table renames from `[policy.module]` to `[policy.component]`.

**5. #207, rebased onto step 2 names and step 4 config.**
Re-key the lookup from `name` to `provides`, verify the claim against `component_type().exports`, refuse duplicate claimants at prepass before either artifact compiles, and enforce the `[implements]` binding and the digest.
Introduces `InterfaceId`.
`[implements]` authorizes **both** services and extension backends from day one, so there is no window where the table describes a load path no artifact can take.
The registry split from `ExtensionBackends` moves to step 9, where the trampolines that would populate it actually land.
Milestone note: #207 is M2 and its epic #204 is M4.

**6.
Fail-closed imports and a per-component linker.
New issue, nothing tracks this.**
`enforce_capabilities` refuses any import it cannot classify against the declared set, instead of ignoring it.
`build_linker` takes the component's resolved dependency set and is called per component, after admission.
`LoadedModule::revive` rebuilds from the same recorded set, so the comment "must match the boot-time linker" stays true.
This is a prerequisite for any service trampoline and it closes a live fail-open hole on its own.
It breaks the public `build_linker` signature, which videre and shepherd tests call.

**7.
`HostCap` and `Gate` in `nexum-world`.**
Non-optional `import` and `adapter` on `HostCap`, `Gate` for http and the wasi gates, `WASI_GATES` splice deleted, hand-written invariant test deleted, four core-set spellings collapsed to two.
Rewrites #39: it is no longer a prerequisite for anything, and its "document the gap" branch is the answer.

**8.
World synthesis for services, both halves.
New issue, nothing tracks this.**
Provider: exports are the provided interface plus `init`, imports are the component's own declared capabilities.
Consumer: `synthesize` learns the interface-id key, emits `import nexum:quotes/feed@1.3.0`, and adds the derived package directory to `ModuleWorld.packages`.
Host: dynamic instantiation plus an `init`-only bindgen world.
The first draft scoped only the provider half and priced only that.
`wit/nexum-host/query-module.wit` is the shape to generalize from.
Required before #205 means anything.

**9. #205, with its grammar and its mechanism rewritten.**
Semver-track matching and every refusal survive.
The alias grammar is replaced by the canonical-key rule.
The resolver sits beside `CapabilityRegistry`, not inside it, with diagnostic-only cross-consultation so that depending on a component name prints the corrected line.
`DependencyKey` lands here as a closed sum: `Host(HostCap)`, `Gate(Gate)`, `Extension(ExtCapName)`, `Service(InterfaceId)`.
The guest-service registry splits from `ExtensionBackends` here.

**10. #206, with added scope.**
Cross-store `Val` trampolines, plus four things the issue does not currently carry: the structural import-versus-export check at boot, refusal of the full store-bound handle set rather than resources alone, the acyclicity refusal on the `[implements]` graph, and fuel, memory and deadline accounting for the hop on both sides.

**11.
One author surface for every kind.
New issue, nothing tracks this.**
Retire `BACKEND_CAPABILITIES`: a service and a backend declare host capabilities in `[dependencies]` like a module, under `[policy]`.
Fix the silent discard at `supervisor/load.rs:495`.
Make the linker's capability set and the manifest-validating namespace derive from one source.

**12.
Issue rewrites.**
#13 splits: its compiled-seam-enum half no longer describes the service path once #207 keys on an interface id, and its `ContentDigest` integrity-tag half survives whole.
#172's worked example keys a dependency by component name, which is what #205 exists to refuse; it moves to the interface-id key, and its open question is answered: a digest is a service-dependency attribute only, because a capability is the runtime and has no artifact.
#127 states that it scopes host and extension capabilities only, and states the absent-record default.
#38 states that it covers host capabilities only.

**13.
Residual type hardening.**
`ExtCapName` with the collision check in its constructor.
`ModuleManifest` and `ServiceManifest` newtypes from prepass, which deletes `LoadRefusal::WorkerKindAdapter`.
`CapabilityRegistry<W>` over a sealed marker, with ZSTs named `ForModule` and `ForBackend`.
Not `ModuleWorld`, which is already a public type in `nexum-world` and would rebuild the one-word-two-concepts problem structurally.
Deferred, possibly forever: renaming `Capability.adapter`.
It feeds `bind_host_via_wit_bindgen!` call sites in every guest crate's glue, and the churn exceeds the confusion.

**Parallel and unaffected.**
#227 and #226 land on the chain seam whenever; the only interaction is a one-file rebase overlap with #224.
The M2 operator-dial train is the operator-dials half of this model and proceeds.
M3, M5 and M6 are untouched.

## Cost

**Nothing merged is discarded.**
Main at 76cc8ea still spells `PROVIDER_CAPABILITIES` and `PROVIDER_NAMESPACE`, and PR #224 is open with `mergedAt: null`.
The window to fix this in review closes when #224 merges.

**Three breaking changes, not one.**
The first draft claimed one.

1. Step 2: the `Extension` trait and the concept 2 types.
   Videre implements `service()`, `provider()` and `HostService` today, so this is a real port.
2. Step 4: `engine.toml`.
   Every operator file needs `id` fields, and `[[services]].http_allow` and the three `[limits]` scalars move.
3. Step 6: `build_linker`'s signature.
   Videre and shepherd call it from tests.

They cannot be merged into one, because step 4 is a config break and step 6 depends on the resolved dependency set that step 5 produces.
They can be announced together.

**Discarded work that this plan itself creates.**
`BACKEND_CAPABILITIES`, `BACKEND_NAMESPACE` and `CapabilityRegistry::backend()` are renamed at step 2 and deleted at step 11.
Keeping them crate-private is what makes that acceptable.

**The tracker.**
Of 56 open issues, 4 need real rewriting, 2 need one added sentence, 2 are obviated independently, and about 40 are untouched.
#204's model is **not** rewritten: `provides` as a verified claim, `[implements]` as operator authorization, interface id as identity, composition rejected because one component is one Store, plain data only across a service edge.
What changes is grammar in #205 and scope in #206 and #207.

**Genuinely new work, none of it tracked.**
Step 6 (fail-closed imports, per-component linker): medium, touches boot, `Shared` and the restart path.
Step 8 (world synthesis, both halves, plus dynamic instantiation): the largest item, several days, and a hard prerequisite for #205.
Step 11 (author surface parity, the `load.rs:495` discard): small in code, but it changes a security-relevant path and needs a migration note, because a service manifest that declares hosts today is silently ignored and would afterwards be intersected or refused.
ADR-0017: half a day.
Amending #224: about a day, on 28 files already written.

**Milestone inversion, stated plainly.**
The plan puts M2 work behind an M4 rename and an unwritten ADR.
#202 is M4 and #224 closes it; #207 is M2 and rebases on it; #152 is M2 and depends on step 4.
There are 17 open M2 issues.
The fix is to relabel #202 and #224 to M2, not to reorder the work.

**Unpriced and unsolved: WIT distribution.**
A consumer must vendor the provider's WIT package by hand.
There is no registry, no fetcher, and no `wit/deps` directory.
#207 scopes WIT distribution out and nothing else scopes it in.
Services are therefore usable inside one group that vendors its own WIT, and not across groups.
This is the single largest gap between the model and a shipping feature.

## Objections and how each was resolved

**Fatal, both fixed.**

1. *Step 2 keeps `service_kind()` returning `BackendInstaller`, and the naming-guard bucket forbids the `SERVICE_WORLD_*` names step 2 chooses.*
   Fixed.
   `Extension::provider()` becomes `backend_installer()`.
   The capability constants become crate-private `BACKEND_*`.
   The guard now has four buckets and no name violates its own bucket.
2. *`[implements]` keys authorization on `[component].name`, the exact ADR-0001 violation the draft cited against #152.*
   Fixed.
   `[[modules]]`, `[[services]]` and `[[backends]]` gain an operator-written `id`.
   `[implements]` and `[policy.component]` both key on it.
   Adding the field is scheduled at step 4.

**Serious, model amended.**

3. *Interface id is a name, not a type; verifying `provides` against exports proves self-consistency only, and wasmtime's semver track is an assumption about well-behaved hosts.*
   Amended.
   A structural instance-type comparison at boot is added to step 10, and the naming-versus-typing distinction is stated in concept 3.
4. *Publishing service trampolines into the shared linker turns the fail-open import walk into a hole.*
   Amended.
   The import walk becomes fail-closed and the linker becomes per component.
   New step 6.
5. *The linker is built before any component compiles, and `revive` rebuilds it from extensions alone.*
   Amended.
   Same step.
   Providers load first in topological order; `revive` rebuilds from the recorded dependency set.
6. *The alias grammar cannot produce the consumer's world: no full version, no package directory.*
   Amended.
   The dependency key is the full interface id, and the package directory is derived by `:` to `-`.
7. *Rejecting composition throws away acyclicity, and a cycle deadlocks a non-reentrant actor lock.*
   Amended.
   An explicit acyclicity refusal on the `[implements]` graph, in step 10.
8. *The alias is not what the author's code calls for a service, so shared item 4 is not shared.*
   Amended.
   The alias is removed.
   The key is the canonical name in both cases, which also removes the two-key-semantics problem.
9. *An operator-visible service has no extension, so the host loses typed bindings for instantiation and `init`.*
   Amended.
   Dynamic instantiation plus an `init`-only bindgen world, scoped and priced in step 8.
10. *Step 7 retires what step 2 lands, so "zero rework" is false.*
    Amended.
    The constants become crate-private, so the discard is internal, and the cost section now says so.
11. *Step 7 retires `[[services]].http_allow` after step 5 freezes the config.*
    Amended.
    One config change, step 4, before anything else touches `engine_config.rs`.
12. *#39 is a WIT vendoring project, not a cleanup, and a `wasi:http` core row breaks two live invariants and forces `NamespaceCaps` to grow a per-row prefix.*
    Amended, and the earlier decision is reversed.
    The type split no longer depends on #39.
    `Gate` models http honestly. #39 becomes "document the gap".
13. *Step 5 ships `[implements]` for a load path no artifact can take until step 10.*
    Amended.
    `[implements]` authorizes extension backends too, from day one.
    The registry split moves to step 9.
14. *`[policy.component."<id>"]` has no join column.*
    Amended by the `id` field.
15. *`[policy]` and `[limits]` both look like the right place for the same dial, and the spellings differ across the seam.*
    Amended.
    `[policy]` supersedes the three scalars, the subsections stay, and both sides spell `max_*`.
16. *`[[services]]` would mean two structurally different things in the operator file.*
    Amended.
    Concept 4 gets `[[backends]]` and `kind = "backend"`.
17. *A backend author still cannot write a manifest without reading the extension's Rust.*
    Amended.
    `BackendInstaller::interface() -> InterfaceId`, so the identifier lives in the extension's WIT.
18. *The consumer half of world synthesis is unscheduled.*
    Amended.
    It is half of step 8.
19. *Structural discrimination makes the commonest author error produce the least useful message.*
    Amended.
    Resolution never crosses tables; the refusal message does.
20. *One interface identity is spelled three ways, and `[implements]` match semantics are unstated.*
    Amended.
    Full semver in WIT-facing places, the compatibility track in `[implements]`, stated once with the reason.
21. *Retiring `http_allow` moves egress scope into the untrusted file.*
    Amended.
    `[policy.component.<id>].http_allow` keeps a per-target operator allowlist.
22. *Two defaults are unstated and the #152 override-key criticism is aimed at a position it does not hold.*
    Amended.
    Absent-record behaviour is stated for all three dials, the criticism is withdrawn, and `kind` becomes required.
23. *The milestone inversion is larger than the one sentence that admits it.*
    Amended.
    Stated with numbers, with relabelling as the recommendation.
24. *Step 3 items are already in #224, and `admit_worker` is a second public break.*
    Amended.
    The duplicates are removed and `admit_worker` folds into step 2.

**Minor, model amended.**

25. *"One linker cannot serve both" is not a wasmtime constraint.*
    Conceded and rewritten.
    The separation is an enforcement-scope decision.
26. *The per-component fuel bound does not survive a service call.*
    Conceded.
    Recorded as a defect for step 10, not as a property already held.
27. *"Resource handles cannot cross" understates the store-bound set.*
    Amended to `resource`, `future`, `stream` and `error-context`.
28. *`ModuleWorld` as a marker ZST collides with the public `nexum-world` type, and the ADR citation is line 90, not 91.*
    Both amended.
    The markers are `ForModule` and `ForBackend`.

**Standing.**

29. **WIT distribution is unsolved.**
    A service edge needs the provider's WIT package in the consumer's `wit/` tree, by hand, with no registry and no fetcher.
    The model needs it and does not supply it.
    Services work inside one group and not across groups until this is answered.
30. **A service edge is not free, and the author surface hides that.**
    `chain = {}` and `"nexum:quotes/feed@1.3.0" = {}` look equally cheap and are not.
    The symmetry is real for declaration and false for effort.
31. **The trust argument for rejecting composition is stated one-sidedly.**
    The write-up lists what composition costs and, apart from acyclicity, not what it buys.
    The rejection is still correct, and the record is still incomplete.
32. **Structural type checking does not prove behaviour.**
    A provider can match the interface exactly and misbehave.
    `[implements]` plus the digest pin is the whole answer, and it rests on the operator knowing what artifact they pinned.
33. **Fuel accounting across a service hop is scoped but not designed.**
    Charging the hop to the caller, bounding call depth, and keeping the callee's own ceiling meaningful are three requirements that may conflict.
    #206 carries the requirement, not a solution.
