# Security policy

## Report a vulnerability

Report a vulnerability through GitHub private vulnerability reporting: [open a draft advisory](https://github.com/nullislabs/nexum-runtime/security/advisories/new).
The report stays private until an advisory is published, so there is no public disclosure before a fix exists.

Do not open a public issue for a vulnerability.
Do not send a report to a maintainer by email, because there is no monitored security inbox.

Include the affected commit or version, the impact, and the steps to reproduce the problem.
A proof-of-concept module or a failing test is the fastest route to a fix.

We aim to acknowledge a report in 5 working days and to give a first assessment in 10 working days.
Keep the report private until the advisory is published.

## Supported versions

nexum-runtime is before its 1.0 release and has no tagged release.
Security fixes land on `main` only.
There is no backport to an earlier commit and no patch release of an older version.

| Version | Supported |
| --- | --- |
| `main` | Yes |
| Any earlier commit | No |

## What is in scope

The runtime supervises untrusted guest components behind a capability gate.
A report is in scope when a guest module can do one of these things:

- Reach a host capability that its manifest does not grant.
- Escape the fuel or memory limit that the host applies to it.
- Reach a network endpoint that the operator allowlist does not permit.
- Read or write the local store of another module.
- Stop the host or another module from making progress.

A report is also in scope when the host leaks operator configuration or another module's data into a module's view.

## What is not in scope

- A denial of service that needs operator-level access to the host.
- A finding that needs a malicious operator configuration, because the operator config is inside the trust boundary.
- A vulnerability in a dependency that already has a published advisory, which the `cargo-deny` CI job reports.
