#!/usr/bin/env bash
#
# Self-check: pre-commit check aligned with GitHub CI.
#
# By default it runs the full CI mirror inside a Linux container
# (scripts/ci-check-docker.sh) — the same checks as every job in
# .github/workflows/ci.yml, including msrv 1.95, coverage (llvm-cov), and
# the ubuntu platform. Add --local to run on the macOS host instead
# (scripts/ci-check.sh): faster, but skips msrv/coverage and runs on macOS
# rather than Linux.
#
# Usage:
#   ./scripts/self-check.sh          # full CI parity via Docker (ubuntu)
#   ./scripts/self-check.sh --local  # macOS host (fast, skips msrv/coverage)
#   ./scripts/self-check.sh --quick  # fast pre-commit: skip tests + frontend
#   ./scripts/self-check.sh --skip-tests
#   ./scripts/self-check.sh --backend
#
# Exit code:
#   0 — all checks pass
#   1 — one or more checks failed
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for arg in "$@"; do
    case "$arg" in
        -h|--help)
            echo "Usage: $0 [--local] [--quick] [--skip-tests] [--backend]"
            echo ""
            echo "  (default)  Full CI parity: runs ci-check-docker.sh (ubuntu:24.04"
            echo "             container, all ci.yml jobs incl. msrv 1.95, coverage,"
            echo "             plugin-boundary). Requires a running Docker daemon"
            echo "             (OrbStack on macOS); the image builds on first run."
            echo "  --local    Run ci-check.sh on the macOS host instead — faster,"
            echo "             but skips msrv/coverage and is not the ubuntu platform."
            echo "  --quick    Skip all cargo test steps and frontend."
            echo "  --skip-tests  Skip all cargo test steps."
            echo "  --backend     Skip frontend (pnpm) steps."
            exit 0
            ;;
    esac
done

LOCAL=false
args=()
for arg in "$@"; do
    case "$arg" in
        --local) LOCAL=true ;;
        --quick) args+=(--skip-tests --backend) ;;
        *) args+=("$arg") ;;
    esac
done

if $LOCAL; then
    "$SCRIPT_DIR/ci-check.sh" "${args[@]}"
else
    "$SCRIPT_DIR/ci-check-docker.sh" "${args[@]}"
fi

# ── Manual checklist (not automated) ────────────────────────────────────────
echo ""
echo "────────────────────────────────────────────────────────────────"
echo "  Manual checklist (not automated)"
echo "────────────────────────────────────────────────────────────────"
echo ""
echo "  □ All tokio::spawn handles registered in TaskRegistry?"
echo "  □ All long-running loops use select! with a shutdown signal?"
echo "  □ Audit log / event send failures logged with warn!?"
echo "  □ Changes covered by tests (unit, integration, E2E)?"
echo ""
