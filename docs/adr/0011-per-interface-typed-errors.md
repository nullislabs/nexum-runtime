---
status: accepted
---

# Each host interface declares its own error over a shared fault vocabulary

## Context

The host once returned one flat envelope from every function and export.
That envelope carried a stringly domain, an error-kind enum, a numeric code, a message and an optional data blob.

It mixed two different things.
The first is the shared failure vocabulary that every interface needs, such as unavailable, timeout and denied.
The second is per-interface detail, such as the node code and decoded bytes of a JSON-RPC revert.

Every interface paid for fields it did not use, and lost the fields it did.
Each module also restated its own identity in every error it built.

## Decision

Follow the WASI idiom.
Each interface declares its own typed error, and those errors share one payload-bearing `fault` vocabulary for the cross-domain cases.

`fault` carries the cases that apply to any interface, such as unsupported, unavailable, denied, rate-limited, timeout, invalid-input and internal.

A richer interface embeds `fault` as one case of its own variant and adds only the cases it needs.
`chain-error` adds a case that carries the node code and the decoded revert bytes.
An interface with nothing to add reports `fault` directly.

The module exports return `result<_, fault>`.

Module identity is the supervisor's business.
The self-naming domain field and the message prefix are therefore gone, and the supervisor derives its metric label and its log kind from the fault case.

The numeric code and the opaque data blob are deleted, along with the flat envelope and every mirror of it.

## Consequences

- A caller matches the typed variant to find the structured cause.
  It does not cross-check a stringly domain against a numeric code.
- The shared cases give one stable label vocabulary for metrics and logs.
  Each interface carries exactly the detail it has.
- A new case in the shared vocabulary reaches the WIT type, the world's fault labels, the guest SDK type and both binding conversions.
  Widening the shared vocabulary is therefore a deliberate act and not a local change.
