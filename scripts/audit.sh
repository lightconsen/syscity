#!/usr/bin/env bash
#
# Security Audit Script for Syscity
#
# Runs the full security audit suite locally:
#   - cargo audit (RustSec advisory database)
#   - pnpm audit (npm advisory database, every pnpm workspace)
#   - cargo deny check (vulns + duplicates + licenses + sources)
#
# Usage:
#   ./audit.sh              # Run full audit
#   ./audit.sh --fix        # Run cargo update before audit
#   ./audit.sh --quick      # Only run cargo audit (skip deny and pnpm audit)
#   ./audit.sh --advisories # Only advisory checks
#   ./audit.sh --cached     # Same checks, no fetches (the offline retry)
#

set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
RESET='\033[0m'

# Flags
FIX=false
QUICK=false
ADVISORIES_ONLY=false
CACHED=false

for arg in "$@"; do
    case "$arg" in
        --fix) FIX=true ;;
        --quick) QUICK=true ;;
        --advisories) ADVISORIES_ONLY=true ;;
        --cached) CACHED=true ;;
        -h|--help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --fix        Run cargo update before auditing"
            echo "  --quick      Only run cargo audit (skip cargo-deny and pnpm audit)"
            echo "  --advisories Only run advisory checks"
            echo "  --cached     No fetches — audit against the local advisory caches"
            echo "  -h, --help   Show this help"
            exit 0
            ;;
    esac
done

# What a cached run trades away: the advisory databases may be a day old. Still
# worth running — real findings fail either way — but only as the retry after a
# first run could not reach GitHub.
AUDIT_FLAGS=""
if $CACHED; then
    AUDIT_FLAGS="--stale --no-fetch"
fi

# Check required tools
check_tool() {
    local tool=$1
    local install_cmd=$2
    if ! command -v "$tool" &>/dev/null; then
        echo -e "${YELLOW}WARN: $tool not found${RESET}"
        echo -e "      Install with: ${BOLD}$install_cmd${RESET}"
        return 1
    fi
    return 0
}

run_step() {
    local name=$1
    shift
    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo -e "${BOLD}$name${RESET}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    if "$@"; then
        echo -e "${GREEN}PASS${RESET}  $name"
        return 0
    else
        echo -e "${RED}FAIL${RESET}  $name"
        return 1
    fi
}

# One workspace's audit, exit status and all. A subshell, so the `cd` does not
# follow the caller out.
audit_one_workspace() {
    (cd "$1" && pnpm audit --audit-level=high)
}

# The advisory check for the JS workspaces.
#
# Everything Rust-only above reads Cargo.lock. The pnpm workspaces are a second
# supply chain with a second advisory feed, and nothing else in this gate looks
# at them — a high advisory there stayed invisible until Dependabot mentioned it
# on a push (source-map-js, GHSA-68fv-2mgg-jv7q).
#
# `pnpm audit` is a query to the registry's advisory endpoint and no cache
# stands in for it, so an unreachable registry warns and carries on: that is an
# outage, not a finding, and failing every commit on it would just teach
# everyone `--no-verify`. Findings *do* fail the run, at `high` — the level
# worth stopping a commit for. Everything below it is Dependabot's to report.
audit_js_workspaces() {
    if ! command -v pnpm &>/dev/null; then
        echo -e "${YELLOW}WARN: pnpm not found — skipping the JS advisory check${RESET}"
        echo -e "      Install with: ${BOLD}corepack enable pnpm${RESET}"
        return 0
    fi
    local lock dir found=false
    for lock in */pnpm-lock.yaml; do
        [ -f "$lock" ] || continue
        found=true
        dir=$(dirname "$lock")
        if ! (cd "$dir" && pnpm ping &>/dev/null); then
            echo -e "${YELLOW}WARN: $dir skipped — npm registry unreachable${RESET}"
            continue
        fi
        run_step "pnpm audit ($dir, high and above)" audit_one_workspace "$dir"
    done
    if [ "$found" = false ]; then
        echo -e "${YELLOW}WARN: no pnpm-lock.yaml found — nothing to audit${RESET}"
    fi
}

# ── Optional: update deps ──────────────────────────────────────────────────
if $FIX; then
    run_step "cargo update" cargo update
fi

# ── cargo audit (RustSec vulnerabilities) ──────────────────────────────────
if check_tool cargo-audit "cargo install cargo-audit"; then
    # Try to fix advisory-db corruption automatically
    if ! cargo audit --version &>/dev/null; then
        echo -e "${YELLOW}  Attempting to repair advisory-db cache...${RESET}"
        rm -rf ~/.cargo/advisory-db 2>/dev/null || true
    fi

    # The flags are fixed literals, so unquoted expansion is what carries them.
    # shellcheck disable=SC2086
    if $ADVISORIES_ONLY; then
        run_step "cargo audit" cargo audit $AUDIT_FLAGS
    else
        run_step "cargo audit (RustSec vulnerabilities)" cargo audit $AUDIT_FLAGS
    fi
else
    if $ADVISORIES_ONLY; then
        exit 1
    fi
fi

# ── pnpm audit (npm advisories, every workspace) ───────────────────────────
# Not on `--quick`: that is the fast loop `scripts/lint.sh` runs, and this one
# needs the network.
if ! $QUICK; then
    audit_js_workspaces
fi

if $ADVISORIES_ONLY; then
    echo ""
    echo -e "${GREEN}Advisory checks complete.${RESET}"
    exit 0
fi

# ── cargo deny (duplicates + licenses + sources + advisories) ──────────────
if $QUICK; then
    echo ""
    echo -e "${GREEN}Quick audit complete.${RESET}"
    exit 0
fi

if check_tool cargo-deny "cargo install cargo-deny"; then
    if $CACHED; then
        run_step "cargo deny check (offline)" cargo deny --offline check
    else
        run_step "cargo deny check (full)" cargo deny check
    fi
else
    exit 1
fi

# ── Summary ────────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}All security checks passed.${RESET}"
echo ""
