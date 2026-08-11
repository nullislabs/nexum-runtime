#!/usr/bin/env bash
# House-style gate. Runs identically in CI and under `just content`.
#
#   content-lint.sh                 check tracked files only
#   content-lint.sh <base>..<head>  also check every commit message in the range
#
# The pull-request body is checked when PR_BODY is set in the environment.
#
# Unlike the editor hook this replaces, nothing is compared against a
# committed baseline: a banned character fails wherever it appears, so a
# violation cannot become the baseline that permits the next one.
set -euo pipefail

status=0
fail() {
    printf '  %s\n' "$1" >&2
    status=1
}

# U+2014 em-dash and U+2013 en-dash. House style takes an ASCII hyphen, a
# colon, or two sentences.
DASHES='\x{2014}|\x{2013}'

# A Conventional Commit subject, optional scope, optional breaking marker.
SUBJECT='^(build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test)(\([a-z0-9._/-]+\))?!?: .+'

# Attribution trailers AGENTS.md forbids by name. Standard agent tooling adds
# the first by default, so this is the check that keeps an unattended
# contribution honest.
FORBIDDEN='Co-Authored-By: Claude|Generated with Claude Code'

echo "content-lint: tracked files"
hits=$(git grep -nIP "$DASHES" -- . || true)
if [ -n "$hits" ]; then
    fail "em-dash or en-dash in tracked files:"
    printf '%s\n' "$hits" | sed 's/^/    /' >&2
fi

if [ -n "${1:-}" ]; then
    echo "content-lint: commit messages in $1"
    while read -r sha; do
        [ -n "$sha" ] || continue
        short=${sha:0:8}
        subject=$(git log -1 --format=%s "$sha")
        body=$(git log -1 --format=%B "$sha")

        if ! printf '%s' "$subject" | grep -qP "$SUBJECT"; then
            fail "$short subject is not a Conventional Commit: $subject"
        fi
        if printf '%s' "$body" | grep -qP "$DASHES"; then
            fail "$short message contains an em-dash or en-dash"
        fi
        if printf '%s' "$body" | grep -qiE "$FORBIDDEN"; then
            fail "$short message carries a forbidden attribution trailer"
        fi
    done < <(git rev-list "$1")
fi

if [ -n "${PR_BODY:-}" ]; then
    echo "content-lint: pull-request body"
    if printf '%s' "$PR_BODY" | grep -qP "$DASHES"; then
        fail "pull-request body contains an em-dash or en-dash"
    fi
    if printf '%s' "$PR_BODY" | grep -qiE "$FORBIDDEN"; then
        fail "pull-request body carries a forbidden attribution trailer"
    fi
fi

if [ "$status" -eq 0 ]; then
    echo "content-lint: ok"
fi
exit "$status"
