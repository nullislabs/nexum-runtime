#!/usr/bin/env bash
# Two rules about `[workspace.dependencies]` that nothing else enforces.
#
# Every key must be inherited by at least one member, or it is dead weight that
# no build ever resolves. cargo-machete cannot see this: it checks a crate's
# manifest against that crate's source, and a virtual workspace's dependency
# table belongs to no crate (bnjbvr/cargo-machete#274).
#
# No `modules/**` member may inherit one. A guest module is a copy-paste
# template for an author who has no access to this table, so it declares its
# external dependencies literally.
#
# A member inherits with `dep.workspace = true` or with
# `dep = { workspace = true, ... }`, in any dependency table including a
# `[target.'cfg(..)']` one, so both spellings count as an inheritor. The match
# is line-shaped: a member that split an inline table over two physical lines
# would read as no inheritor. Nothing in the tree does that, and the failure
# direction is the safe one.

set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

status=0
fail() {
    printf '  %s\n' "$1" >&2
    status=1
}

# axum is declared with no inheritor on purpose: nullislabs/nexum-runtime#147
# owns the decision, because the declaration is its evidence that the health
# endpoint was reserved for axum.
EXEMPT="axum"

INHERITS='(\.workspace[[:space:]]*=[[:space:]]*true|=[[:space:]]*\{[^}]*workspace[[:space:]]*=[[:space:]]*true)'

mapfile -t members < <(git ls-files '*Cargo.toml' | grep -vx 'Cargo.toml')
# grep with no file operand reads stdin, so an empty list would hang the job
# rather than fail it.
if [ "${#members[@]}" -eq 0 ]; then
    fail "found no member manifests to check"
    exit 1
fi

# A sub-table spells its key in the header; every other key is the left-hand
# side of an assignment in the flat table.
mapfile -t keys < <(awk '
    /^\[workspace\.dependencies\][[:space:]]*(#.*)?$/ { flat = 1; next }
    /^\[workspace\.dependencies\./ {
        flat = 0
        sub(/^\[workspace\.dependencies\./, "")
        sub(/\].*$/, "")
        print
        next
    }
    /^\[/ { flat = 0 }
    flat && /^[A-Za-z0-9_-]+[[:space:]]*=/ { sub(/[[:space:]]*=.*$/, ""); print }
' Cargo.toml)
# Reading nothing means the table moved or was respelled, not that it is clean.
# Without this the whole check reports ok while inspecting no key at all.
if [ "${#keys[@]}" -eq 0 ]; then
    fail "read no keys from [workspace.dependencies] in Cargo.toml"
    exit 1
fi

echo "workspace-deps-lint: ${#keys[@]} workspace dependencies, ${#members[@]} member manifests"
for key in "${keys[@]}"; do
    mapfile -t inheritors < <(grep -lE "^[[:space:]]*${key}[[:space:]]*${INHERITS}" "${members[@]}" || true)
    case " $EXEMPT " in
    *" $key "*) ;;
    *)
        if [ "${#inheritors[@]}" -eq 0 ]; then
            fail "no member inherits [workspace.dependencies] $key"
        fi
        ;;
    esac
    for member in "${inheritors[@]}"; do
        case "$member" in
        modules/*) fail "$member inherits $key from [workspace.dependencies]; a guest module declares its external dependencies literally" ;;
        esac
    done
done

if [ "$status" -eq 0 ]; then
    echo "workspace-deps-lint: ok"
fi
exit "$status"
