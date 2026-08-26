#!/usr/bin/env bash
# Two rules on `[workspace.dependencies]` nothing else enforces: every key has
# an inheritor, and no `modules/**` member inherits one. cargo-machete sees
# neither, since the table belongs to no crate (bnjbvr/cargo-machete#274).
#
# The inheritor match is line-shaped, so an inline table split over two lines
# reads as none. Nothing does that, and it fails loud.

set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

status=0
fail() {
    printf '  %s\n' "$1" >&2
    status=1
}

INHERITS='(\.workspace[[:space:]]*=[[:space:]]*true|=[[:space:]]*\{[^}]*workspace[[:space:]]*=[[:space:]]*true)'

mapfile -t members < <(git ls-files '*Cargo.toml' | grep -vx 'Cargo.toml')
# grep with no file operand reads stdin, so an empty list would hang.
if [ "${#members[@]}" -eq 0 ]; then
    fail "found no member manifests to check"
    exit 1
fi

# A sub-table spells its key in the header; the rest assign in the flat table.
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
# Reading nothing means the table moved, not that it is clean.
if [ "${#keys[@]}" -eq 0 ]; then
    fail "read no keys from [workspace.dependencies] in Cargo.toml"
    exit 1
fi

echo "workspace-deps-lint: ${#keys[@]} workspace dependencies, ${#members[@]} member manifests"
for key in "${keys[@]}"; do
    mapfile -t inheritors < <(grep -lE "^[[:space:]]*${key}[[:space:]]*${INHERITS}" "${members[@]}" || true)
    if [ "${#inheritors[@]}" -eq 0 ]; then
        fail "no member inherits [workspace.dependencies] $key"
    fi
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
