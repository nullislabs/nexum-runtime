#!/usr/bin/env bash
# Every `[workspace.dependencies]` key must be inherited by at least one member,
# or it is dead weight that no build ever resolves. cargo-machete cannot see
# this: it checks a crate's manifest against that crate's source, and a virtual
# workspace's dependency table belongs to no crate (bnjbvr/cargo-machete#274).
#
# A member inherits with `dep.workspace = true` or with
# `dep = { workspace = true, ... }`, in any dependency table including a
# `[target.'cfg(..)']` one, so both spellings count as an inheritor.

set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

# axum is declared with no inheritor on purpose: nullislabs/nexum-runtime#147
# owns the decision, because the declaration is its evidence that the health
# endpoint was reserved for axum.
EXEMPT="axum"

status=0
mapfile -t members < <(git ls-files '*Cargo.toml' | grep -vx 'Cargo.toml')
# grep with no file operand reads stdin, so an empty list would hang the job
# rather than fail it.
if [ "${#members[@]}" -eq 0 ]; then
    echo "workspace-deps-lint: found no member manifests to check" >&2
    exit 1
fi

while read -r key; do
    [ -n "$key" ] || continue
    case " $EXEMPT " in *" $key "*) continue ;; esac
    if ! grep -qE "^[[:space:]]*${key}[[:space:]]*(\.workspace[[:space:]]*=[[:space:]]*true|=[[:space:]]*\{[^}]*workspace[[:space:]]*=[[:space:]]*true)" "${members[@]}"; then
        printf '  no member inherits [workspace.dependencies] %s\n' "$key" >&2
        status=1
    fi
done < <(awk '/^\[workspace\.dependencies\]$/{f=1;next} /^\[/{f=0} f' Cargo.toml |
    grep -oE '^[A-Za-z0-9_-]+[[:space:]]*=' | tr -d ' =')

if [ "$status" -eq 0 ]; then
    echo "workspace-deps-lint: every workspace dependency has an inheritor"
fi
exit "$status"
