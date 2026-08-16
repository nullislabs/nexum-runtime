#!/usr/bin/env bash
# House-style gate. Runs identically in CI and under `just content`.
#
#   content-lint.sh                 check tracked and untracked files
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

# Drop fenced code blocks and inline code spans from a message before
# checking it. Prose that quotes a banned string in order to explain the rule
# is not a use of it, and a rule that cannot be written down is unusable. Code
# spans carry the quoted form, so removing them separates the two cases
# without guessing at intent.
strip_code() {
    # SC2016: the backticks are markdown fences and code spans, matched
    # literally. Single quotes are correct here and expansion is not wanted.
    # shellcheck disable=SC2016
    sed -e '/^[[:space:]]*```/,/^[[:space:]]*```/d' -e 's/`[^`]*`//g'
}

# U+2014 em-dash and U+2013 en-dash. House style takes an ASCII hyphen, a
# colon, or two sentences.
DASHES='\x{2014}|\x{2013}'

# A Conventional Commit subject, optional scope, optional breaking marker.
SUBJECT='^(build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test)(\([a-z0-9._/-]+\))?!?: .+'

# Oxford spelling takes -ize. Each stem requires a suffix, so the nouns
# "synthesis" and "analysis" cannot match; words with no -ize form (supervise,
# exercise, otherwise, promise) are absent by construction, not excluded after
# the fact. Every match is case-insensitive, because a sentence-initial
# capital is the same violation.
IZE_STEMS='authoris|capitalis|categoris|centralis|customis|decentralis|deserialis|formalis|generalis|initialis|materialis|maximis|minimis|normalis|optimis|organis|prioritis|realis|recognis|sanitis|serialis|stabilis|standardis|summaris|synthesis|tokenis|utilis'
ISE='('"$IZE_STEMS"')(e|es|ed|ing|ation|ations)\b'

# Attribution trailers AGENTS.md forbids by name. Standard agent tooling adds
# the first by default, so this is the check that keeps an unattended
# contribution honest.
FORBIDDEN='Co-Authored-By: Claude|Generated with Claude Code'

# `--untracked` covers a new file before it is staged, which is when the
# author can still fix it cheaply. Ignored paths stay out, so `target` and
# the nix store are not walked.
echo "content-lint: tracked and untracked files"
hits=$(git grep --untracked -nIP "$DASHES" -- . || true)
if [ -n "$hits" ]; then
    fail "em-dash or en-dash in tracked or untracked files:"
    printf '%s\n' "$hits" | sed 's/^/    /' >&2
fi

# CONTRIBUTING.md is exempt: stating the rule means quoting the spelling it
# rejects.
hits=$(git grep --untracked -nIPi "$ISE" -- . ':!CONTRIBUTING.md' || true)
if [ -n "$hits" ]; then
    fail "Oxford spelling takes -ize, not -ise:"
    printf '%s\n' "$hits" | sed 's/^/    /' >&2
fi

if [ -n "${1:-}" ]; then
    echo "content-lint: commit messages in $1"
    while read -r sha; do
        [ -n "$sha" ] || continue
        short=${sha:0:8}
        subject=$(git log -1 --format=%s "$sha")
        body=$(git log -1 --format=%B "$sha" | strip_code)

        if ! printf '%s' "$subject" | grep -qP "$SUBJECT"; then
            fail "$short subject is not a Conventional Commit: $subject"
        fi
        if printf '%s' "$body" | grep -qP "$DASHES"; then
            fail "$short message contains an em-dash or en-dash"
        fi
        if printf '%s' "$body" | grep -qiE "$FORBIDDEN"; then
            fail "$short message carries a forbidden attribution trailer"
        fi
        if printf '%s' "$body" | grep -qPi "$ISE"; then
            fail "$short message uses -ise where Oxford spelling takes -ize"
        fi
    done < <(git rev-list "$1")
fi

if [ -n "${PR_BODY:-}" ]; then
    echo "content-lint: pull-request body"
    prose=$(printf '%s' "$PR_BODY" | strip_code)
    if printf '%s' "$prose" | grep -qP "$DASHES"; then
        fail "pull-request body contains an em-dash or en-dash"
    fi
    if printf '%s' "$prose" | grep -qiE "$FORBIDDEN"; then
        fail "pull-request body carries a forbidden attribution trailer"
    fi
    if printf '%s' "$prose" | grep -qPi "$ISE"; then
        fail "pull-request body uses -ise where Oxford spelling takes -ize"
    fi
fi

if [ "$status" -eq 0 ]; then
    echo "content-lint: ok"
fi
exit "$status"
