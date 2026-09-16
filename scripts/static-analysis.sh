#!/usr/bin/env bash
#
# Static analysis for recurring Syscity correctness anti-patterns.
#
# By default (no flag): scans only src/gateway/, all findings are errors.
#   This is the CI path — exit 1 on any violation.
#
# With --full: scans the entire src/ tree.
#   High-risk modules produce errors (exit 1).
#   Other modules produce warnings only.
#   Known-safe fire-and-forget patterns are excluded.
#
# Usage:
#   ./scripts/static-analysis.sh          # CI mode (gateway only)
#   ./scripts/static-analysis.sh --full   # Full tree scan
#   ./scripts/static-analysis.sh --skip-lock  # CI mode without lock checking
#

set -euo pipefail

RED='\033[0;31m'
YELLOW='\033[1;33m'
RESET='\033[0m'

failures=0
warnings=0

# ── CLI argument parsing ───────────────────────────────────────────────────

FULL=false
SKIP_LOCK=false
for arg in "$@"; do
    case "$arg" in
        --full) FULL=true ;;
        --skip-lock) SKIP_LOCK=true ;;
        -h|--help)
            echo "Usage: $0 [--full] [--skip-lock]"
            echo ""
            echo "  (no flag)    CI mode — src/gateway/ only, errors fail the build"
            echo "  --full       Full scan — high-risk modules are errors, rest are warnings"
            echo "  --skip-lock  Skip lock().await checking (reduces noise)"
            exit 0
            ;;
    esac
done

# ── Allowlist for known-safe patterns (used in --full mode) ────────────────
#
# These are fire-and-forget operations that are intentionally best-effort:
# temp file cleanup, process lifecycle, pipe I/O, env var reads, parsing, etc.
SAFE_PATTERNS='(remove_file|remove_dir_all|child\.wait\(\)|\.kill\(\)|stdin\.write_all|stdin\.shutdown|tx\.send\(|server\.await|\.recv\(\)|env::var\(|\.parse::<|\.metadata\(\)|ctrl_c\(\)|try_get\(|create_dir_all)'

# ── analyze() — shared check function ──────────────────────────────────────
#
# Arguments:
#   $1 = grep pattern (ERE)
#   $2 = human-readable message
#   $3 = file glob(s) for git grep (colon-separated like 'dir/*.rs:dir/**/*.rs')
#   $4 = severity: "error" (default) or "warning"
#   $5 = exclude pattern (ERE) — lines matching this are filtered out
#
analyze() {
    local pattern=$1
    local message=$2
    local files=$3
    local severity=${4:-error}
    local exclude_pattern=${5:-}

    # NB: $files is intentionally unquoted. In default mode it's a single
    #     colon-separated glob ("dir/*.rs:dir/**/*.rs") — no spaces, one word.
    #     In --full mode it's space-separated directory paths that must
    #     undergo word splitting so each becomes a separate git grep arg.
    local matches
    # shellcheck disable=SC2086
    matches=$(git grep -n -E "$pattern" -- $files 2>/dev/null || true)
    if [[ -n "$exclude_pattern" ]]; then
        matches=$(echo "$matches" | grep -v -E "$exclude_pattern" || true)
    fi

    if [[ -n "$matches" ]]; then
        echo ""
        if [[ "$severity" == "error" ]]; then
            echo -e "${RED}ERROR:${RESET} $message"
            failures=$((failures + 1))
        else
            echo -e "${YELLOW}WARNING:${RESET} $message"
            warnings=$((warnings + 1))
        fi
        echo -e "${YELLOW}Matches:${RESET}"
        echo "$matches"
    fi
}

# ── Module classification (--full mode) ────────────────────────────────────
#
# High-risk: core runtime paths where silent error drops can cause data loss.
# Rest:      utilities, drivers, CLI, and other infra where fire-and-forget
#            (temp cleanup, etc.) is acceptable.
HIGH_RISK_DIRS='src/agent/ src/channels/ src/memory/ src/cron/ src/security/ src/acp/ src/inbound/ src/outbound/ src/gateway/'
REST_DIRS='src/adapters/ src/browser/ src/canvas/ src/cli/ src/computer/ src/core/ src/device/ src/heartbeat/ src/model_router/ src/perception/ src/planner/ src/plugins/ src/providers/ src/skills/ src/standing_orders/ src/tools/ src/tui/ src/utils/'

# ── Pattern definitions ────────────────────────────────────────────────────
#
# Each entry: (grep_pattern, message, files, severity_for_full)
# The CI mode always uses "error" severity and gateway-only files.

PATTERN_LET_AWAIT='let _ = .*\.await;?$'
MSG_LET_AWAIT="Found 'let _ = ... .await' — handle or log the error instead of discarding it."

PATTERN_DOT_OK='\.ok\(\);?$'
MSG_DOT_OK="Found trailing '.ok()' that discards errors. Use match or if-let."

PATTERN_SPAWN='tokio::spawn\('
MSG_SPAWN="Found direct tokio::spawn — consider registering the handle in TaskRegistry."

PATTERN_LOCK_AWAIT='\.lock\(\).*\.await'
MSG_LOCK_AWAIT="Possible std::sync::Mutex held across await. Use tokio::sync::Mutex or scope the lock."

PATTERN_SLEEP_LOOP='loop \{.*tokio::time::sleep'
MSG_SLEEP_LOOP="Possible busy-wait loop using tokio::time::sleep without shutdown select."

GATEWAY_FILES='src/gateway/*.rs:src/gateway/**/*.rs'

# ── Default mode (CI) — gateway only, all errors ───────────────────────────
#
# Exact same behavior as before the --full flag was added.
if ! $FULL; then
    analyze "$PATTERN_LET_AWAIT" "$MSG_LET_AWAIT" "$GATEWAY_FILES"
    analyze "$PATTERN_DOT_OK" "$MSG_DOT_OK" "$GATEWAY_FILES"
    analyze "$PATTERN_SPAWN" "$MSG_SPAWN" "$GATEWAY_FILES"
    if ! $SKIP_LOCK; then
        analyze "$PATTERN_LOCK_AWAIT" "$MSG_LOCK_AWAIT" "$GATEWAY_FILES" "warning"
    fi
    analyze "$PATTERN_SLEEP_LOOP" "$MSG_SLEEP_LOOP" "$GATEWAY_FILES"

    if [[ $failures -gt 0 ]]; then
        echo ""
        echo -e "${RED}Static analysis failed with $failures issue(s).${RESET}"
        exit 1
    fi
    echo "Static analysis passed."
    exit 0
fi

# ── --full mode — scan entire src/ tree ────────────────────────────────────

echo "── Full static analysis ──"
echo "  High-risk modules (errors):  agent, channels, memory, cron, security, acp, inbound, outbound, gateway"
echo "  Other modules (warnings):    adapters, browser, canvas, cli, computer, core, device, heartbeat,"
echo "                               model_router, perception, planner, plugins, providers, skills,"
echo "                               standing_orders, tools, tui, utils"
echo ""

do_full_scan() {
    local pattern=$1
    local message=$2

    # High-risk modules → error
    analyze "$pattern" "(HIGH) $message" "$HIGH_RISK_DIRS" "error" "$SAFE_PATTERNS"

    # Rest of src → warning only
    analyze "$pattern" "(LOW)  $message" "$REST_DIRS" "warning" "$SAFE_PATTERNS"
}

do_full_scan "$PATTERN_LET_AWAIT" "$MSG_LET_AWAIT"
do_full_scan "$PATTERN_DOT_OK" "$MSG_DOT_OK"
do_full_scan "$PATTERN_SPAWN" "$MSG_SPAWN"

# Lock pattern — always warning (cannot distinguish std::sync::Mutex from
# tokio::sync::Mutex at the grep level; all ~33 real hits use tokio::Mutex).
if ! $SKIP_LOCK; then
    analyze "$PATTERN_LOCK_AWAIT" "(HIGH) $MSG_LOCK_AWAIT" "$HIGH_RISK_DIRS" "warning" "$SAFE_PATTERNS"
    analyze "$PATTERN_LOCK_AWAIT" "(LOW)  $MSG_LOCK_AWAIT" "$REST_DIRS" "warning" "$SAFE_PATTERNS"
fi

do_full_scan "$PATTERN_SLEEP_LOOP" "$MSG_SLEEP_LOOP"

# ── Invariant declaration convention (#16) ──────────────────────────────────
#
# Every top-level module must either own runtime invariants registered with
# `core::invariants` (see src/core/invariants.rs, surfaced through
# `syscity invariants`) or carry an explicit `INVARIANTS-NONE:` marker
# explaining why it holds none. Nothing is silently unchecked.
# Delegated to a standalone script so CI enforces the same rule; do not
# duplicate the logic here.

_INVARIANT_CHECK="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-invariant-decls.sh"
if ! "$_INVARIANT_CHECK" >/dev/null 2>&1; then
    "$_INVARIANT_CHECK"
    failures=$((failures + 1))
fi

# ── Summary ────────────────────────────────────────────────────────────────

echo ""
if [[ $warnings -gt 0 ]]; then
    echo -e "${YELLOW}$warnings warning(s) found in low-risk modules (review manually).${RESET}"
fi
if [[ $failures -gt 0 ]]; then
    echo ""
    echo -e "${RED}Static analysis failed with $failures high-risk issue(s).${RESET}"
    exit 1
fi
echo "Static analysis passed (--full)."
