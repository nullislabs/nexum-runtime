#!/usr/bin/env bash
# Enforces the settlement-domain-agnostic claim in the `nexum-runtime` crate
# rustdoc: no domain symbol in the runtime crate or `wit/`, `nexum:host` stays
# a leaf WIT package, and no crate edge leaves the repository. Reads text and
# compiles nothing.
#
# No baseline file: a leak fails wherever it appears, so a violation cannot
# become the baseline that permits the next one.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

status=0
fail() {
    printf '  %s\n' "$1" >&2
    status=1
}

# The downstream repositories, per README.md: their names are the domain
# vocabulary a leak would carry.
DOMAIN='videre|shepherd'

PACKAGE='nexum:host'

# Every crate and every module, not just the runtime crate: a published
# example naming a downstream is the same category error as the engine
# doing it, and `description` reaches crates.io.
echo "zero-leak: domain symbols in crates, modules and wit"
hits=$(git grep -nIiE "$DOMAIN" -- crates modules wit || true)
if [ -n "$hits" ]; then
    fail "domain symbol or WIT reference in a first-party source:"
    printf '%s\n' "$hits" | sed 's/^/    /' >&2
fi

echo "zero-leak: $PACKAGE is a leaf WIT package"
while read -r wit; do
    [ -n "$wit" ] || continue
    if ! grep -qE "^package[[:space:]]+${PACKAGE}(@|;)" "$wit"; then
        fail "$wit does not declare the $PACKAGE package"
    fi
    # A foreign import carries a package id, so a colon precedes the
    # interface; a local `use types.{...}` has none.
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
