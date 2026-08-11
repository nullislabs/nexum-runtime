#!/usr/bin/env bash
# Venue-agnostic gate. Runs identically in CI and under `just zero-leak`.
#
#   zero-leak.sh    check tracked files only
#
# The crate rustdoc of `nexum-runtime` claims the runtime is
# settlement-domain-agnostic: no domain symbol or WIT reference, `nexum:host`
# stays a leaf WIT package, and no crate edge reaches a domain crate. This
# script is the enforcement that sentence names. It reads text and compiles
# nothing, in the style of `content-lint.sh`.
#
# Three checks, one per clause:
#
#   1. No downstream domain namespace appears in `crates/nexum-runtime` or in
#      `wit/`. `README.md` names videre and shepherd as the repositories that
#      build on this one, so their names are the domain vocabulary a leak
#      would carry.
#   2. Every WIT file declares the `nexum:host` package and imports no other
#      package. A leaf package resolves with no dependency on disk.
#   3. No manifest in the workspace takes a git dependency, and no manifest
#      path escapes the repository. Both are how an out-of-tree domain crate
#      would become an edge of this graph.
#
# Nothing is compared against a committed baseline: a leak fails wherever it
# appears, so a violation cannot become the baseline that permits the next
# one.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

status=0
fail() {
    printf '  %s\n' "$1" >&2
    status=1
}

# The downstream repositories of the Nullis runtime stack, per README.md.
# Their names stand for the settlement domain this crate must not know.
DOMAIN='videre|shepherd'

# The one WIT package this repository owns.
PACKAGE='nexum:host'

echo "zero-leak: domain symbols in crates/nexum-runtime and wit"
hits=$(git grep -nIiE "$DOMAIN" -- crates/nexum-runtime wit || true)
if [ -n "$hits" ]; then
    fail "domain symbol or WIT reference in the runtime crate:"
    printf '%s\n' "$hits" | sed 's/^/    /' >&2
fi

echo "zero-leak: $PACKAGE is a leaf WIT package"
while read -r wit; do
    [ -n "$wit" ] || continue
    if ! grep -qE "^package[[:space:]]+${PACKAGE}(@|;)" "$wit"; then
        fail "$wit does not declare the $PACKAGE package"
    fi
    # A local `use types.{...}` names an interface in this package. A foreign
    # import carries a package id, so it holds a colon before the interface.
    foreign=$(grep -nE '^[[:space:]]*(use|include)[[:space:]]+[a-z0-9_-]+:' "$wit" || true)
    if [ -n "$foreign" ]; then
        fail "$wit imports a package outside $PACKAGE:"
        printf '%s\n' "$foreign" | sed 's/^/    /' >&2
    fi
done < <(git ls-files 'wit/*.wit')

echo "zero-leak: crate edges stay in the repository"
while read -r manifest; do
    [ -n "$manifest" ] || continue
    gits=$(grep -nE '(^|[[:space:]{,])git[[:space:]]*=' "$manifest" || true)
    if [ -n "$gits" ]; then
        fail "$manifest takes a git dependency:"
        printf '%s\n' "$gits" | sed 's/^/    /' >&2
    fi
    dir=$(dirname "$manifest")
    while read -r path; do
        [ -n "$path" ] || continue
        resolved=$(realpath -m --relative-to="$root" "$dir/$path")
        case "$resolved" in
        ..* | /*) fail "$manifest points outside the repository: $path" ;;
        esac
    done < <(grep -oE 'path[[:space:]]*=[[:space:]]*"[^"]+"' "$manifest" |
        sed -e 's/.*"\(.*\)"/\1/')
done < <(git ls-files '*Cargo.toml')

if [ "$status" -eq 0 ]; then
    echo "zero-leak: ok"
fi
exit "$status"
