#!/usr/bin/env bash
# tui-tmux-smoke.sh — drive the real TUI inside a real terminal multiplexer.
#
# The PTY suite (tests/tui_pty.rs) runs the TUI on a pseudo-terminal it
# controls itself, so it cannot prove anything about how a *terminal
# emulator* in between renders the session — DECSTBM scroll regions, wide
# CJK cells, the cursor query. tmux is such an emulator: it translates the
# escape stream itself. This script runs the whole loop inside a detached
# tmux session and asserts on what tmux's own grid captured:
#
#   1. the composer is pinned at the bottom row of the pane;
#   2. a CJK echo comes back tight (the scrollback writes through tmux's
#      scroll-region translation — the old cell-dump path would show
#      "你 好 小 王" here);
#   3. /quit leaves tmux with a usable pane (exit status printed).
#
# Everything is throwaway: a private SYSCITY_HOME, a private port, a detached
# tmux session that is killed on exit. Usage: scripts/tui-tmux-smoke.sh
# [path-to-syscity-binary] [port].

set -euo pipefail

BIN="${1:-target/debug/syscity}"
PORT="${2:-18799}"
SESSION="syscity-tui-smoke-$$"
HOME_DIR="$(mktemp -d /tmp/syscity-tmux-smoke.XXXXXX)"
GATEWAY_PID=""

fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

cleanup() {
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    [ -n "$GATEWAY_PID" ] && kill "$GATEWAY_PID" 2>/dev/null || true
    rm -rf "$HOME_DIR"
}
trap cleanup EXIT

command -v tmux >/dev/null || fail "tmux not installed"
[ -x "$BIN" ] || fail "no binary at $BIN — cargo build first"

NO_PROXY="127.0.0.1,localhost,::1" no_proxy="127.0.0.1,localhost,::1" \
    SYSCITY_HOME="$HOME_DIR" "$BIN" start --host 127.0.0.1 --port "$PORT" --foreground \
    >"$HOME_DIR/gateway.log" 2>&1 &
GATEWAY_PID=$!

# Wait for the gateway to answer before pointing a TUI at it.
for _ in $(seq 1 50); do
    curl -sf --noproxy "*" "http://127.0.0.1:$PORT/live" >/dev/null 2>&1 && break
    sleep 0.2
done
curl -sf --noproxy "*" "http://127.0.0.1:$PORT/live" >/dev/null 2>&1 || {
    cat "$HOME_DIR/gateway.log" >&2
    fail "gateway never came up on $PORT"
}

# The TUI talks to 127.0.0.1; a proxy in the environment would swallow that.
tmux new-session -d -s "$SESSION" -x 100 -y 30 \
    "NO_PROXY='127.0.0.1,localhost,::1' no_proxy='127.0.0.1,localhost,::1' \
     SYSCITY_HOME='$HOME_DIR' '$BIN' tui --host 127.0.0.1 --port $PORT; echo TUI-EXIT=\$?; sleep 300"

# Wait for the composer to be up (the gateway handshake + first frame).
pane=""
for _ in $(seq 1 50); do
    pane="$(tmux capture-pane -p -t "$SESSION")"
    [[ "$pane" == *"connected"* ]] && break
    sleep 0.2
done
[[ "$pane" == *"connected"* ]] || fail "no greeting: $pane"

# 1. The input line is the pane's last row — pinned to the bottom.
bottom="$(tmux capture-pane -p -t "$SESSION" | grep -v '^$' | tail -1)"
[[ "$bottom" == "> "* || "$bottom" == ">" ]] \
    || fail "composer is not the bottom row: $bottom"

# 2. CJK stays tight through tmux's scroll-region translation.
tmux send-keys -t "$SESSION" "你好小王很稀疏吗" Enter
pane=""
for _ in $(seq 1 50); do
    pane="$(tmux capture-pane -p -t "$SESSION")"
    [[ "$pane" == *"你好小王很稀疏吗"* ]] && break
    sleep 0.2
done
[[ "$pane" == *"> 你好小王很稀疏吗"* ]] || fail "CJK echo not tight: $pane"

# 3. /quit exits cleanly and says so. Escape first: the CJK line above started
# a turn that no provider will ever answer, and a stopped run is the honest
# precondition for quitting.
tmux send-keys -t "$SESSION" Escape
sleep 0.5
tmux send-keys -t "$SESSION" "/quit" Enter
for _ in $(seq 1 50); do
    pane="$(tmux capture-pane -p -t "$SESSION")"
    [[ "$pane" == *"TUI-EXIT=0"* ]] && break
    sleep 0.2
done
[[ "$pane" == *"TUI-EXIT=0"* ]] || fail "TUI did not exit 0: $pane"

echo "tmux smoke OK (port $PORT)"
