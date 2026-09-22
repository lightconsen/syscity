#!/usr/bin/env bash
#
# CI Check — mirror the GitHub Actions CI pipeline locally.
#
# Replicates every job in .github/workflows/ci.yml (frontend, check, msrv,
# test, test-local-embeddings, test-macos, build, security, coverage,
# plugin-boundary) so a push that passes here is very likely to pass CI.
#
# Steps that need a tool/toolchain not installed locally are skipped with an
# install hint rather than failing — but everything that CAN run, RUNS.
#
# Usage:
#   ./scripts/ci-check.sh               # full CI mirror (default)
#   ./scripts/ci-check.sh --skip-tests  # skip all cargo test steps
#   ./scripts/ci-check.sh --backend     # Rust steps only (no frontend)
#
# Exit code:
#   0 — every check passes
#   1 — one or more checks failed
#
set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RESET='\033[0m'

RUN_TESTS=true
RUN_FRONTEND=true
# A missing tool is a *skip* by default so a local run tells you what to install
# rather than blocking on it. In CI — or with --strict — it is a failure: a
# green run that silently omitted the audit and the coverage gate is worse than
# a red one, because nothing downstream can tell the difference.
STRICT=false
[[ -n "${CI:-}" ]] && STRICT=true

for arg in "$@"; do
    case "$arg" in
        --skip-tests) RUN_TESTS=false ;;
        --backend) RUN_FRONTEND=false ;;
        --strict) STRICT=true ;;
        -h|--help)
            echo "Usage: $0 [--skip-tests] [--backend] [--strict]"
            echo ""
            echo "  (default)     Mirror of .github/workflows/ci.yml: frontend,"
            echo "                fmt, clippy, static-analysis, cargo check, msrv,"
            echo "                tests (unit/E2E/doc/local-embeddings), docs,"
            echo "                security audit, coverage, plugin boundary"
            echo "  --skip-tests  Skip all cargo test steps"
            echo "  --backend     Skip frontend (pnpm) steps"
            echo "  --strict      Fail (instead of skipping) when a tool is missing."
            echo "                Always on when CI is set in the environment."
            exit 0
            ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

errors=0
skipped=0

check() {
    local name=$1
    shift
    echo -e "${YELLOW}[${name}]${RESET} $*"
    if "$@" 2>&1; then
        echo -e "${GREEN}[${name}] passed${RESET}"
    else
        echo -e "${RED}[${name}] FAILED${RESET}"
        errors=$((errors + 1))
    fi
    echo ""
}

# Run a command with `working-directory: web` (as ci.yml does).
in_dir() {
    local dir=$1
    shift
    ( cd "$dir" && "$@" )
}

# Skip a step whose tool/toolchain is not installed (matches self-check.sh).
skip_with_hint() {
    local name=$1
    shift
    if $STRICT; then
        echo -e "${RED}[${name}] FAILED${RESET} $*"
        echo -e "${RED}[${name}] $1 is not installed, and --strict (or CI) is set:"
        echo -e "${RED}          this step must run, not be skipped. Install it.${RESET}"
        errors=$((errors + 1))
        echo ""
        return
    fi
    echo -e "${YELLOW}[${name}]${RESET} $*"
    echo -e "${YELLOW}[${name}] skipped (${RESET}$1${YELLOW} not installed — install it to match CI)${RESET}"
    skipped=$((skipped + 1))
    echo ""
}

# ── Job: frontend (ci.yml `frontend`) ───────────────────────────────────────
if $RUN_FRONTEND; then
    check "frontend: install" in_dir web pnpm install --frozen-lockfile
    check "frontend: build" in_dir web pnpm run build
    check "frontend: typecheck" in_dir web npx tsc --noEmit
fi

# ── Job: check (ci.yml `check`) ─────────────────────────────────────────────
check "fmt" cargo fmt -- --check
check "clippy" cargo +stable clippy --all-features -- -D warnings
check "static-analysis" ./scripts/static-analysis.sh
check "cargo check (all-features)" cargo check --all-features
# Benches, examples and test targets are not built by `cargo check`/`clippy`, so
# this is the only local step that catches a bench rotting against a moved API.
check "cargo check (all-targets)" cargo check --all-targets
check "cargo doc" cargo doc --no-deps --all-features

# ── Job: msrv (ci.yml `msrv`) ───────────────────────────────────────────────
if cargo +1.94 --version >/dev/null 2>&1; then
    check "msrv (1.94)" cargo +1.94 check --all-features
else
    skip_with_hint "msrv (1.94)" "rustup toolchain install 1.94 --profile minimal"
fi

# ── Job: build (ci.yml `build`: default + all-features) ─────────────────────
check "cargo check (default)" cargo check

# ── Jobs: test + test-macos (ci.yml `test` / `test-macos`) ─────────────────
if $RUN_TESTS; then
    check "tests (all-features)" cargo test --all-features -- --skip e2e:: --nocapture
    # The chat journeys run against the mock provider when no real one is
    # configured, so they belong here. `browser_chat_tests` is the exception:
    # it needs a real Chrome, and self-skips without one — a self-skip inside a
    # green test run is exactly the kind of "passed by not running" this list
    # should keep visible.
    check "tests (e2e mock/no-provider)" \
        cargo test --test e2e_test -- \
        --skip browser_chat_tests \
        --test-threads=1
    check "tests (doc)" cargo test --doc --all-features
fi

# ── Job: test-local-embeddings (ci.yml `test-local-embeddings`) ─────────────
if $RUN_TESTS; then
    check "tests (local-embeddings)" cargo test --features local-embeddings -- --skip e2e::
fi

# ── Job: security (ci.yml `security`) ───────────────────────────────────────
if command -v cargo-audit >/dev/null 2>&1; then
    check "cargo audit" cargo audit
else
    skip_with_hint "cargo audit" "cargo install cargo-audit"
fi
if command -v cargo-deny >/dev/null 2>&1; then
    check "cargo deny" cargo deny check
else
    skip_with_hint "cargo deny" "cargo install cargo-deny"
fi

# ── Job: coverage (ci.yml `coverage`; Codecov upload is CI-only) ────────────
if command -v cargo-llvm-cov >/dev/null 2>&1; then
    check "coverage (llvm-cov)" cargo llvm-cov --workspace --exclude syscity-desktop --all-features --summary-only --fail-under-lines 55 --ignore-filename-regex '(^|/)(tests|target)/' -- --skip e2e::
else
    skip_with_hint "coverage (llvm-cov)" "cargo install cargo-llvm-cov && rustup component add llvm-tools-preview"
fi

# ── Job: plugin-boundary (ci.yml `plugin-boundary`) ─────────────────────────
check "plugin-boundary" env SYSCITY_PLUGIN_DIRS="./plugins" ./scripts/check-plugin-boundary.sh

# ── Summary ────────────────────────────────────────────────────────────────
if [[ $errors -gt 0 ]]; then
    echo -e "${RED}$errors check(s) failed. Fix them before pushing.${RESET}"
    echo ""
    exit 1
fi

echo -e "${GREEN}All CI checks passed locally.${RESET}"
if [[ $skipped -gt 0 ]]; then
    echo -e "${YELLOW}  ($skipped step(s) skipped — install the tools above to match CI fully,"
    echo -e "${YELLOW}   or re-run with --strict to treat them as failures)${RESET}"
fi
