#!/usr/bin/env bash
# Verifies that every ONNX Runtime session is pinned to the CPU execution
# provider.
#
# `ort`'s `Session::builder` sets ONNX Runtime's EP selection policy to
# `MaxEfficiency` (= `PreferNPU`). On macOS that registers CoreML, and CoreML
# computes the CRNN recognizer's squeeze-and-excitation branch wrongly: input
# tensors wider than roughly 320px decode as all-blank, while the same bytes
# read correctly through the CPU provider. The chinese preset (6625 classes)
# is wide enough to hit it; the english feature map collapses before it does.
#
# `with_execution_providers` does NOT undo this. A manually registered EP runs
# alongside the automatic one rather than displacing it; only
# `AutoDevicePolicy::PreferCPU` registers nothing.
#
# Three ways to get this wrong, all rejected here:
#   1. A session committed outside the one helper that sets the policy.
#   2. A `Session::builder` that never sets the policy (so it inherits
#      `MaxEfficiency` again).
#   3. The helper itself losing the `PreferCPU` line — every session then goes
#      through it and inherits the wrong policy silently, and checks 1 and 2
#      would both pass while nothing was pinned.
#
# Exit 0 = every session is pinned; exit 1 = list what is not.
set -euo pipefail

HELPER='src/computer/vision/mod.rs'
# The policy, not the whole call: the path is qualified as
# `ort::session::builder::AutoDevicePolicy::PreferCPU`, and pinning on the
# full expression made this guard fail on a tree that was correct.
POLICY_EXPR='AutoDevicePolicy::PreferCPU'

failures=0

# ── 1. No session may be committed outside the helper ──────────────────────
commit_sites=$(git grep -n -E 'commit_from_(file|memory|bytes)\(' -- 'src/*.rs' 'src/**/*.rs' 2>/dev/null \
    | grep -v "^${HELPER}:" || true)
if [[ -n "$commit_sites" ]]; then
    echo "ONNX sessions committed outside ${HELPER}:" >&2
    echo "$commit_sites" | sed 's/^/  /' >&2
    echo "  → use computer::vision::cpu_only_session(), which pins the CPU EP." >&2
    failures=$((failures + 1))
fi

# ── 2. Every builder in the helper sets the policy ─────────────────────────
# A builder is wrong when the next few lines do not carry the policy call.
unpinned=$(git grep -n -E 'Session::builder\(' -- "$HELPER" 2>/dev/null \
    | cut -d: -f2 \
    | while read -r line; do
        window=$(sed -n "${line},$((line + 8))p" "$HELPER")
        if ! grep -qF "$POLICY_EXPR" <<<"$window" || ! grep -qF 'with_auto_device' <<<"$window"; then
            echo "${HELPER}:${line}"
        fi
    done)
if [[ -n "$unpinned" ]]; then
    echo "Session builders that do not set AutoDevicePolicy::PreferCPU:" >&2
    echo "$unpinned" | sed 's/^/  /' >&2
    failures=$((failures + 1))
fi

# ── 3. The helper itself still pins ───────────────────────────────────────
if ! git grep -qF "$POLICY_EXPR" -- "$HELPER" 2>/dev/null; then
    echo "${HELPER} no longer sets ${POLICY_EXPR} — every ONNX session" >&2
    echo "now inherits ort's MaxEfficiency policy (CoreML on macOS)." >&2
    failures=$((failures + 1))
fi

# ── 4. Bypasses the helper by asking for the raw environment ───────────────
env_users=$(git grep -n -E 'ort_environment\(\)' -- 'src/*.rs' 'src/**/*.rs' 2>/dev/null \
    | grep -v "^${HELPER}:" || true)
if [[ -n "$env_users" ]]; then
    echo "ort_environment() used outside ${HELPER}:" >&2
    echo "$env_users" | sed 's/^/  /' >&2
    echo "  → a session built from it skips the CPU pin; use cpu_only_session()." >&2
    failures=$((failures + 1))
fi

if [[ $failures -gt 0 ]]; then
    exit 1
fi
echo "every ONNX session is pinned to the CPU execution provider"
