#!/usr/bin/env bash
# Every `crates/**` member inherits `[workspace.lints]`; `modules/**` carries no
# such rule yet. Cargo never warns on a member that omits the table, so a crate
# opts out of `unwrap_used` and `missing_docs` in silence.
#
# The match is line-shaped, so an inline `lints = { workspace = true }` reads as
# none. Nothing does that, and it fails loud.

set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

status=0
fail() {
    printf '  %s\n' "$1" >&2
    status=1
}

mapfile -t members < <(git ls-files 'crates/*/Cargo.toml')
# Reading nothing means the layout moved, not that it is clean.
if [ "${#members[@]}" -eq 0 ]; then
    fail "found no crates/ manifests to check"
    exit 1
fi

echo "crate-lints-lint: ${#members[@]} crate manifests"
for member in "${members[@]}"; do
    awk '
        /^\[lints\][[:space:]]*(#.*)?$/ { in_lints = 1; next }
        /^\[/ { in_lints = 0 }
        in_lints && /^workspace[[:space:]]*=[[:space:]]*true[[:space:]]*(#.*)?$/ { found = 1 }
        END { exit !found }
    ' "$member" || fail "$member does not carry [lints] workspace = true"
done

if [ "$status" -eq 0 ]; then
    echo "crate-lints-lint: ok"
fi
exit "$status"
