#!/usr/bin/env bash
# Three files state the toolchain version independently and none of them is
# compiled, so any one can drift without an error: Cargo.toml, the flake pin,
# and the CI action input. The action names two components, which rustup
# resolves to the newest patch, so it is compared on major.minor.
#
# The licence rides along because cargo performs no SPDX validation at all:
# `cargo publish --dry-run` packages and compiles a member licensed
# `NOT-A-REAL-LICENCE` and exits 0.

set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

status=0
fail() {
    printf '  %s\n' "$1" >&2
    status=1
}

MANIFEST=Cargo.toml
FLAKE=flake.nix
ACTION=.github/actions/rust-setup/action.yml

# `|| true`: a missing key must reach the report, not die on grep's exit status.
msrv=$(grep -oE '^rust-version = "[0-9.]+"' "$MANIFEST" | sed -e 's/.*"\(.*\)"/\1/' || true)
flake=$(grep -oE 'rust-bin\.stable\."[0-9.]+"' "$FLAKE" | sed -e 's/.*"\(.*\)"/\1/' || true)
action=$(grep -oE 'toolchain: "[0-9.]+"' "$ACTION" | sed -e 's/.*"\(.*\)"/\1/' || true)

echo "msrv-lint: $MANIFEST $msrv, $FLAKE $flake, $ACTION $action"

for pair in "$MANIFEST:$msrv" "$FLAKE:$flake" "$ACTION:$action"; do
    if [ -z "${pair#*:}" ]; then
        fail "${pair%%:*} states no Rust version, or states it in a form this check cannot read"
    fi
done

if [ "$status" -eq 0 ]; then
    if [ "$msrv" != "$flake" ]; then
        fail "$MANIFEST rust-version $msrv does not match the $FLAKE pin $flake"
    fi
    if [ "$(cut -d. -f1,2 <<<"$action")" != "$(cut -d. -f1,2 <<<"$flake")" ]; then
        fail "$ACTION toolchain $action does not match the $FLAKE pin $flake"
    fi
fi

# Matched by shape, not against this workspace's choice, so a licence change
# needs no edit here.
license=$(grep -oE '^license = "[^"]+"' "$MANIFEST" | sed -e 's/.*"\(.*\)"/\1/' || true)
echo "msrv-lint: $MANIFEST licence $license"
if [ -z "$license" ]; then
    fail "$MANIFEST states no license on [workspace.package]"
elif [[ "$license" =~ ^(AGPL|LGPL|GPL)-[0-9]+\.[0-9]+$ ]]; then
    fail "$MANIFEST license $license is deprecated in the SPDX list; write $license-only or $license-or-later"
fi

echo "msrv-lint: every member inherits rust-version and license"
while read -r member; do
    [ -n "$member" ] || continue
    [ "$member" = "$MANIFEST" ] && continue
    for key in rust-version license; do
        if ! grep -qE "^$key\.workspace = true\$" "$member"; then
            fail "$member does not carry $key.workspace = true"
        fi
    done
done < <(git ls-files '*Cargo.toml')

if [ "$status" -eq 0 ]; then
    echo "msrv-lint: ok"
fi
exit "$status"
