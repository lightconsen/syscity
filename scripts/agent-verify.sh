#!/usr/bin/env bash
#
# Agent Verify — bounded verification for the nightly agent loop.
#
# Runs the fast half of the CI gate (the steps of ci.yml's `check` job) plus
# cargo tests scoped to the modules the diff touches. This lets the agent loop
# refuse to open a PR on a red tree without paying for the full CI matrix; the
# full gate still runs on the PR itself (ci.yml).
#
# Usage:
#   ./scripts/agent-verify.sh [--base <ref>] [--full-tests]
#
#   --base <ref>    Git ref to diff against (default: origin/main).
#   --full-tests    Run the whole test suite instead of the scoped subset.
#
# Exit code:
#   0 — every check passes
#   1 — one or more checks failed
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RESET='\033[0m'

BASE="origin/main"
FULL_TESTS=false
for arg in "$@"; do
    case "$arg" in
        --base) shift; BASE="${1:-origin/main}" ;;
        --full-tests) FULL_TESTS=true ;;
        -h|--help)
            echo "Usage: $0 [--base <ref>] [--full-tests]"
            echo ""
            echo "  --base <ref>    Git ref to diff against (default: origin/main)"
            echo "  --full-tests    Run the whole test suite instead of the scoped subset"
            exit 0
            ;;
    esac
done

if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
    echo -e "${YELLOW}[agent-verify]${RESET} '$BASE' does not resolve; falling back to 'main'"
    BASE="main"
fi

errors=0

check() {
    local name=$1
    shift
    echo -e "${YELLOW}[agent-verify:${name}]${RESET} $*"
    if "$@"; then
        echo -e "${GREEN}[agent-verify:${name}] passed${RESET}"
    else
        echo -e "${RED}[agent-verify:${name}] FAILED${RESET}"
        errors=$((errors + 1))
    fi
    echo ""
}

# ── Static gate: the steps of ci.yml's `check` job ─────────────────────────
check "fmt" cargo fmt -- --check
check "clippy" cargo clippy --all-features -- -D warnings
check "static-analysis" ./scripts/static-analysis.sh
check "gate-integrity" ./scripts/gate-integrity.sh --tree
check "cargo-check" cargo check --all-features
check "cargo-doc" cargo doc --no-deps --all-features

# ── Scoped tests: only the modules the diff touches ────────────────────────
changed="$(git diff --name-only "$BASE" || true)"
modules="$(printf '%s\n' "$changed" | sed -nE 's#^src/([^/]+)/.*#\1#p' | sort -u)"
module_count="$(printf '%s\n' "$modules" | grep -c . || true)"

# A change to a root-level Cargo.toml or src/*.rs can affect anything, so it
# widens the blast radius past what a module filter can bound.
wide=false
if printf '%s\n' "$changed" | grep -qE '^(Cargo\.toml|src/[^/]+\.rs)$'; then
    wide=true
fi

if [ "$module_count" -eq 0 ]; then
    echo -e "${YELLOW}[agent-verify:tests]${RESET} no src/ changes — skipping tests"
elif $FULL_TESTS || $wide || [ "$module_count" -gt 3 ]; then
    if $FULL_TESTS; then
        reason="--full-tests"
    elif $wide; then
        reason="root-file change"
    else
        reason=">3 modules"
    fi
    echo -e "${YELLOW}[agent-verify:tests]${RESET} full suite (${reason})"
    check "tests" cargo test --all-features -- --skip e2e::
else
    for m in $modules; do
        check "tests:${m}" cargo test --all-features -- "${m}::" --skip e2e::
    done
fi

# ── Summary ────────────────────────────────────────────────────────────────
if [ "$errors" -gt 0 ]; then
    echo -e "${RED}[agent-verify] ${errors} check(s) failed.${RESET}"
    exit 1
fi
echo -e "${GREEN}[agent-verify] all checks passed.${RESET}"
