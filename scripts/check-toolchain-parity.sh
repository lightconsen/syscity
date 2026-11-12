#!/usr/bin/env bash
#
# Toolchain parity: the toolchain this repository *declares* is the one every
# script actually runs on.
#
# `scripts/ci-check.sh` used to say `cargo +stable clippy` while CI pinned
# 1.97. `+stable` on a developer machine was 1.99, which lints code 1.97
# accepts, so the local "CI mirror" went red on ten findings CI would never
# report — and the mirror, the one thing whose job is to predict CI, became
# the least trustworthy gate in the repo.
#
# This check closes that class of drift: every `cargo +<toolchain>` invocation
# in scripts/ must name the pinned channel, and the pinned channel must agree
# with Cargo.toml's `rust-version` and with every `toolchain:` line in ci.yml.
#
# Usage: scripts/check-toolchain-parity.sh
# Exit 0 = all declarations agree; exit 1 = list the disagreeing files.

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
RESET='\033[0m'

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$ROOT" ]; then
    echo "check-toolchain-parity: must run inside a git repository" >&2
    exit 1
fi
cd "$ROOT"

errors=0
fail() {
    echo -e "${RED}FAIL${RESET}  $1"
    errors=$((errors + 1))
}
pass() { echo -e "${GREEN}PASS${RESET}  $1"; }

toolchain=$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' rust-toolchain.toml | head -1)
if [ -z "$toolchain" ]; then
    fail "rust-toolchain.toml has no 'channel = \"...\"' line"
    echo ""
    echo "cannot continue without the pinned channel"
    exit 1
fi

# ── 1. Cargo.toml rust-version agrees with the pinned channel ──────────────
rust_version=$(sed -n 's/^rust-version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -1)
if [ "$rust_version" != "$toolchain" ]; then
    fail "Cargo.toml rust-version ($rust_version) != rust-toolchain.toml channel ($toolchain)"
else
    pass "Cargo.toml rust-version = $toolchain"
fi

# ── 2. Every 'toolchain:' line in ci.yml names the pinned channel ──────────
ci_versions=$(sed -n 's/^ *toolchain:[[:space:]]*"\(.*\)"/\1/p' .github/workflows/ci.yml | sort -u)
if [ -z "$ci_versions" ]; then
    fail ".github/workflows/ci.yml declares no 'toolchain:' lines"
else
    for v in $ci_versions; do
        if [ "$v" != "$toolchain" ]; then
            fail "ci.yml uses toolchain $v, pinned is $toolchain"
        fi
    done
    [ "$errors" -eq 0 ] && pass "ci.yml toolchain(s) = $toolchain"
fi

# ── 3. Every 'cargo +<toolchain>' in scripts/ uses the pinned channel ──────
#
# `+stable`, `+beta` and `+nightly` are the interesting failures: they resolve
# to whatever the machine happens to have, so the check runs on a toolchain
# nobody declared. A bare `cargo` is correct by construction — it honours
# rust-toolchain.toml — and is not matched.
bad=""
# Walk files explicitly: `grep -n` then supplies only the line number, and the
# comment-stripped body is scanned for `cargo +<name>` anywhere in the line
# (not just at the start — `check "clippy" cargo +stable clippy` puts it in
# the middle).
while IFS= read -r file; do
    while IFS= read -r row; do
        [ -n "$row" ] || continue
        lineno=${row%%:*}
        body=${row#*:}
        # Skip comment lines: a script explaining "this used to say
        # `cargo +stable`" must not fail its own check.
        case "${body#"${body%%[![:space:]]*}"}" in
            '#'*) continue ;;
        esac
        # One name per line; `grep -o` also splits multiple hits on one line.
        while IFS= read -r name; do
            [ -n "$name" ] || continue
            if [ "$name" != "$toolchain" ]; then
                bad+="       $file:$lineno (cargo +$name)"$'\n'
            fi
        done < <(printf '%s\n' "$body" | grep -oE 'cargo \+[A-Za-z0-9._-]+' | sed 's/^cargo +//')
    done < <(grep -nE 'cargo \+[A-Za-z0-9._-]+' "$file" 2>/dev/null || true)
done < <(find scripts -name '*.sh' -type f 2>/dev/null | sort)
if [ -n "$bad" ]; then
    fail "scripts/ run a toolchain other than the pinned $toolchain:"
    printf '%s' "$bad"
    echo "       Use bare 'cargo' (honours rust-toolchain.toml) or 'cargo +$toolchain'."
else
    pass "every 'cargo +<toolchain>' in scripts/ names $toolchain"
fi

echo ""
if [ "$errors" -gt 0 ]; then
    echo -e "${RED}${errors} toolchain-parity check(s) failed.${RESET}"
    exit 1
fi
exit 0
