#!/usr/bin/env bash
#
# Build the Android app, install it, and check that the second display really
# works on this device: the helper starts, a display is created, an app is
# launched onto it, a frame comes back, a tap lands, and the display tears down.
#
# This is the first checkpoint for the second-screen work. The helper runs as
# the shell user (uid 2000) and creates a virtual display through a hidden API
# by reflection; launching an app onto a non-default display and reading a frame
# from it are claims about the platform that only a device can settle.
#
# The check drives the same tool actions an agent would (`android_display`
# over the gateway's WebSocket), which is the only path that works: the helper
# refuses to start without the handoff nonce the app issues, so it cannot be
# launched by hand with a `CLASSPATH` and a class name.
#
# Usage:
#   ./scripts/android-display-check.sh              # build (debug), install, check
#   ./scripts/android-display-check.sh --skip-build # use the last APK built
#   ./scripts/android-display-check.sh --release    # release APK instead
#   ./scripts/android-display-check.sh --device emulator-5554
#   ./scripts/android-display-check.sh --port 18080 # gateway port to drive
#
# Exit code:
#   0 — every step answered
#   1 — a step failed; the message names it

set -euo pipefail

RED=$'\033[0;31m'
GREEN=$'\033[0;32m'
YELLOW=$'\033[1;33m'
BLUE=$'\033[0;34m'
RESET=$'\033[0m'

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
PACKAGE="net.syscity.desktop"
APK_DIR="$REPO_ROOT/desktop/gen/android/app/build/outputs/apk"

BUILD_PROFILE="debug"
SKIP_BUILD=false
DEVICE="${ANDROID_SERIAL:-}"
PORT="${SYSCITY_PORT:-18080}"

log() { printf '%s==>%s %s\n' "$BLUE" "$RESET" "$*"; }
ok() { printf '%s ok %s %s\n' "$GREEN" "$RESET" "$*"; }
warn() { printf '%swarn%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() {
    printf '%sfail%s %s\n' "$RED" "$RESET" "$*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --skip-build) SKIP_BUILD=true; shift ;;
        --debug) BUILD_PROFILE="debug"; shift ;;
        --release) BUILD_PROFILE="release"; shift ;;
        --device)
            [ $# -ge 2 ] || die "--device needs a serial"
            DEVICE="$2"
            shift 2
            ;;
        --port)
            [ $# -ge 2 ] || die "--port needs a number"
            PORT="$2"
            shift 2
            ;;
        -h | --help)
            awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

# ── 0. adb, a device, and a way to drive the gateway ───────────────────────
command -v adb >/dev/null 2>&1 ||
    die "adb is not on PATH — install the Android platform tools (sdkmanager \
\"platform-tools\") and put them on PATH"
command -v node >/dev/null 2>&1 ||
    die "node is not on PATH — the check drives the gateway's WebSocket, and \
the client is a small node script"

if [ -z "$DEVICE" ]; then
    # One device, so nothing is ambiguous. Two is a choice this script has no
    # business making silently: `adb` would pick one for it and the log would
    # not say which.
    mapfile -t attached < <(adb devices | awk 'NR > 1 && $2 == "device" { print $1 }')
    case "${#attached[@]}" in
        0) die "no device or emulator attached (start one, or pass --device)" ;;
        1) DEVICE="${attached[0]}" ;;
        *) die "several devices attached (${attached[*]}) — pass --device SERIAL" ;;
    esac
fi

adb_cmd() { adb -s "$DEVICE" "$@"; }

adb_cmd get-state >/dev/null 2>&1 || die "device $DEVICE is not reachable"
ok "device $DEVICE ($(adb_cmd shell getprop ro.build.version.sdk | tr -d '\r') API)"

API=$(adb_cmd shell getprop ro.build.version.sdk | tr -d '\r')
if [ "${API:-0}" -lt 34 ]; then
    warn "API $API: the display works, but without per-display focus (API 34)"
    warn "      typing on it can take the user's keyboard"
fi

# ── 1. build ───────────────────────────────────────────────────────────────
if $SKIP_BUILD; then
    log "build skipped (--skip-build)"
else
    log "building the $BUILD_PROFILE APK (this is also the first time the"
    log "      Kotlin is compiled — a compile error surfaces here, not on device)"
    "$REPO_ROOT/scripts/android-build.sh" "--$BUILD_PROFILE"
    ok "build"
fi

# The APK path depends on the ABI and profile, so take the newest rather than
# guessing a layout that changes with the Tauri CLI.
APK=$(find "$APK_DIR" -name '*.apk' -type f -print0 2>/dev/null |
    xargs -0 -r ls -t 2>/dev/null | head -1 || true)
[ -n "$APK" ] || die "no APK under $APK_DIR — run without --skip-build"
ok "apk $(basename "$APK") ($(du -h "$APK" | cut -f1))"

# ── 2. install and start ───────────────────────────────────────────────────
log "installing"
adb_cmd install -r -t "$APK" >/dev/null || die "adb install failed (a signature \
mismatch with an existing install needs 'adb uninstall $PACKAGE' first)"
ok "installed $PACKAGE"

# A reinstall clears the accessibility service, and without it the display's
# tree cannot be read at all — `DISPLAY_TREE_UNAVAILABLE`, which reads like a
# broken display. Setting it back is part of what installing costs.
log "starting the app and re-enabling the accessibility service"
adb_cmd shell am start -n "$PACKAGE/.MainActivity" >/dev/null 2>&1 || true
sleep 5
adb_cmd shell settings put secure enabled_accessibility_services \
    "$PACKAGE/.SyscityAccessibilityService" >/dev/null 2>&1 || true
adb_cmd shell settings put secure accessibility_enabled 1 >/dev/null 2>&1 || true

FORWARDED=$(adb_cmd forward --list 2>/dev/null | grep -c "tcp:$PORT" || true)
if [ "${FORWARDED:-0}" -eq 0 ]; then
    log "forwarding tcp:$PORT to the app's gateway"
    adb_cmd forward "tcp:$PORT" "tcp:$PORT" >/dev/null
fi

# The per-install token, not the host config's `shared_token`: the gateway runs
# inside the app and enforces its own.
TOKEN=$(adb_cmd shell run-as "$PACKAGE" cat "files/syscity/data/gateway_token" 2>/dev/null |
    tr -d '\r')
[ -n "$TOKEN" ] || die "no gateway token on the device — is the app installed \
and has it started once?"
ok "gateway token read from the app"

# ── 3. the sequence, through the gateway ───────────────────────────────────
log "driving android_display / android_input over the gateway's WebSocket"
set +e
RESULT=$(SYSCITY_TOKEN="$TOKEN" SYSCITY_PORT="$PORT" node --input-type=module -e '
const port = process.env.SYSCITY_PORT;
const token = process.env.SYSCITY_TOKEN;
const base = `http://127.0.0.1:${port}`;

const ticketAnswer = await fetch(`${base}/api/v1/ws-ticket`, {
  method: "POST",
  headers: { Authorization: `Bearer ${token}` },
});
const { ticket } = await ticketAnswer.json();

const ws = new WebSocket(`ws://127.0.0.1:${port}/ws?ticket=${ticket}`);
await new Promise((done, failed) => {
  ws.addEventListener("open", done, { once: true });
  ws.addEventListener("error", failed, { once: true });
});

const waiting = new Map();
ws.addEventListener("message", (event) => {
  const message = JSON.parse(event.data);
  const finish = waiting.get(message.id);
  if (finish) { waiting.delete(message.id); finish(message); }
});
// Everything is bounded: a tool that never answers must fail the check with a
// message, not leave it hanging with no output at all.
const request = (id, method, params, timeoutMs = 90000) => new Promise((resolve, reject) => {
  const timer = setTimeout(() => {
    waiting.delete(id);
    reject(new Error(`\`${method}\` (${id}) did not answer within ${timeoutMs} ms`));
  }, timeoutMs);
  waiting.set(id, (answer) => { clearTimeout(timer); resolve(answer); });
  ws.send(JSON.stringify({ type: "req", id, method, params }));
});

await request("connect", "connect", {
  protocol_version: 1, client: { id: "android-display-check" },
});
const tool = async (id, name, args) => {
  const answer = await request(id, "debug.tool.invoke", { tool: name, arguments: args });
  return answer.payload ?? {};
};
const say = (step, text) => console.log(`${step}: ${text}`);
const stop = (reason) => { console.log(`FAIL ${reason}`); ws.close(); process.exit(1); };

const status = await tool("status", "android_display", { action: "status" });
if (status.error || status.success === false) {
  say("helper", "not attached yet — `start` brings it up");
} else {
  say("helper", "answering");
}

const limits = await tool("limits", "android_display", { action: "limits" });
const data = limits.data ?? {};
if (limits.error || limits.success === false) {
  // `limits` reads from the helper and does not start one, so right after a
  // reinstall it cannot answer yet. That is not a failure: `start` is about to
  // bring the helper up, and it is the call that would be refused if no slot
  // were free.
  say("limits", `not answerable until the helper starts (${limits.error})`);
} else {
  say("limits", `${data.limit} display(s) may be open, ${data.open} are` +
    (data.reason ? ` — ${data.reason}` : ""));
  if (typeof data.remaining === "number" && data.remaining < 1) {
    // Every slot is taken — by a display this run did not open, so closing it
    // would be closing work this run did not open. Say so instead of failing at
    // `start`.
    stop("limits: all " + data.limit + " display(s) are in use; close one first " +
      "(android_display action: stop display: <id>)");
  }
}

// The first helper call also starts the helper, through the loopback-ADB
// pairing the app owns — which is not ready the instant the app is launched. A
// refused call created nothing, so retrying is safe; it is the wait that is
// missing, not a fix.
let started = await tool("start", "android_display", { action: "start" });
for (let attempt = 1; started.data?.display === undefined && attempt <= 10; attempt += 1) {
  say("start", `attempt ${attempt} failed: ${started.error ?? "no display id"}`);
  await new Promise((r) => setTimeout(r, 4000));
  started = await tool(`start-${attempt}`, "android_display", { action: "start" });
}
const display = started.data?.display;
if (display === undefined) {
  stop(`create: ${started.error ?? started.output ?? JSON.stringify(started)}`);
}
say("display", `id=${display} ${started.data?.width}x${started.data?.height}`);

const launched = await tool("launch", "android_display", {
  action: "launch", display, package: "com.android.settings",
});
if (launched.error) stop(`launch: ${launched.output}`);
say("launched", "com.android.settings");
await new Promise((r) => setTimeout(r, 2500));

const frame = await tool("frame", "android_display", { action: "frame", display });
if (!frame.data?.base64) stop(`frame: ${frame.output ?? "no picture"}`);
say("frame", `${frame.data.width}x${frame.data.height} png=` +
  `${frame.data.base64.length} base64 chars`);

const tapped = await tool("tap", "android_input", {
  action: "tap", display, x: 540, y: 1200, verify: true,
});
if (tapped.error) stop(`tap: ${tapped.output}`);
say("tap", `(540, 1200) — effect ${tapped.data?.effect ?? "unknown"}`);

const stopped = await tool("stop", "android_display", { action: "stop", display });
if (stopped.error) stop(`stop: ${stopped.output}`);
say("stopped", `display ${display}`);
console.log("CHECK OK");
// `ws.close()` alone is not enough to end the process: the ticket fetch leaves
// a keep-alive socket open, so the script would print its verdict and then
// hang, holding the caller for as long as that socket lives. Exiting is what
// makes the verdict reach the shell.
ws.close();
process.exit(0);
' 2>&1)
CLIENT_RC=$?
set -e
printf '%s\n' "$RESULT"

# ── 4. verdict ─────────────────────────────────────────────────────────────
# The client prints its verdict last and exits. Believing the last line is what
# keeps a killed run from being read as a pass: it can still have printed
# `CHECK OK` before it was interrupted, but only a run that reached the end
# prints it last.
if [ "$(printf '%s' "$RESULT" | tail -1)" = "CHECK OK" ]; then
    echo ""
    ok "second display works on this device"
    exit 0
fi
# A `FAIL <step>` line is the client naming what broke; fall through to the
# case below so the message stays specific. Anything else means it died without
# saying why, or was killed, and that is not a diagnosis.
if ! printf '%s' "$RESULT" | grep -q '^FAIL '; then
    printf '%s\n' "$RESULT" >&2
    die "the gateway client stopped early (exit $CLIENT_RC) without naming a \
step; output above"
fi
case "$RESULT" in
    *"CREATE_FAILED"* | *"create:"*)
        die "creating the display failed. The helper reports what the hidden \
createVirtualDisplay calls returned; that message is version-specific."
        ;;
    *"LAUNCH_FAILED"* | *"launch:"*)
        die "the display exists but nothing would start on it. Check \
'adb shell am start --display <id> -n <component>' by hand, and that the target \
package is installed."
        ;;
    *"NO_FRAME"* | *"frame:"*)
        die "the display rendered nothing. 'adb shell dumpsys display' should list \
a DisplayDevice named syscity-agent; if it is there but blank, the app never \
reached the display."
        ;;
    *"NO_HELPER"* | *"helper:"*)
        die "the display helper is not running. Start it with \
'android_display action: restart', or check that the app is running and the \
loopback-ADB pairing is up."
        ;;
    *"DISPLAY_TREE_UNAVAILABLE"*)
        die "the accessibility service is off, so the display cannot be read. \
Re-enable $PACKAGE in Settings › Accessibility and run again."
        ;;
    *)
        die "the check did not finish; output above"
        ;;
esac
