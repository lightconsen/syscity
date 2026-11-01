#!/usr/bin/env bash
#
# Build the Android app, install it, and run the display helper's self-test on
# a device or emulator.
#
# This is the first checkpoint for the second-screen work: the helper runs as
# the shell user (uid 2000), creates a virtual display through a hidden API by
# reflection, launches an app onto it, reads a frame and taps it. Every one of
# those is a claim about the platform that only a device can settle, and none of
# them needs the app to be running — only installed, because the helper's class
# lives in the APK.
#
# Usage:
#   ./scripts/android-display-check.sh              # build (debug), install, self-test
#   ./scripts/android-display-check.sh --skip-build # use the last APK built
#   ./scripts/android-display-check.sh --release    # release APK instead
#   ./scripts/android-display-check.sh --device emulator-5554
#
# Exit code:
#   0 — the self-test printed SELFTEST OK
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
        -h | --help)
            awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

# ── 0. adb and a device ────────────────────────────────────────────────────
command -v adb >/dev/null 2>&1 ||
    die "adb is not on PATH — install the Android platform tools (sdkmanager \
\"platform-tools\") and put them on PATH"

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
if [ "${API:-0}" -lt 30 ]; then
    warn "API $API: a second display's accessibility tree needs API 30; the"
    warn "      frame-based path still works, but half of what this checks will not"
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

# ── 2. install ─────────────────────────────────────────────────────────────
log "installing"
adb_cmd install -r -t "$APK" >/dev/null || die "adb install failed (a signature \
mismatch with an existing install needs 'adb uninstall $PACKAGE' first)"
ok "installed $PACKAGE"

ON_DEVICE=$(adb_cmd shell pm path "$PACKAGE" | head -1 | sed 's/^package://' | tr -d '\r')
[ -n "$ON_DEVICE" ] || die "$PACKAGE is installed but pm gave no APK path"
ok "device APK $ON_DEVICE"

# ── 3. self-test ───────────────────────────────────────────────────────────
# The app must NOT be running for a clean test: a running instance holds its own
# views and its log noise buries the helper's. Force-stop is harmless (the app
# keeps no important state across a restart).
adb_cmd shell am force-stop "$PACKAGE" || true

# Where the user's screen is focused, before and after. The self-test taps on a
# display of its own, so a change here means the tap landed on the wrong one —
# the failure this whole exercise exists to catch.
focus() { adb_cmd shell dumpsys window 2>/dev/null | grep -m1 'mCurrentFocus' || true; }
FOCUS_BEFORE=$(focus)

log "running the helper self-test"
set +e
OUTPUT=$(adb_cmd shell "CLASSPATH=$ON_DEVICE app_process /system/bin \
net.syscity.desktop.helper.HelperMain --selftest" 2>&1)
RC=$?
set -e

printf '%s\n' "$OUTPUT"

FOCUS_AFTER=$(focus)
if [ -n "$FOCUS_BEFORE" ] && [ "$FOCUS_BEFORE" != "$FOCUS_AFTER" ]; then
    warn "the user's focused window changed during the test:"
    warn "  before: $FOCUS_BEFORE"
    warn "  after:  $FOCUS_AFTER"
    warn "  on API 34+ this should not happen (FLAG_OWN_FOCUS); if it did, input"
    warn "  is reaching the wrong display — check the 'tap' step below"
fi

# ── 4. verdict ─────────────────────────────────────────────────────────────
if printf '%s' "$OUTPUT" | grep -q 'SELFTEST OK'; then
    echo ""
    ok "second display works on this device"
    exit 0
fi

echo ""
case "$OUTPUT" in
    *"FAIL create"*)
        die "creating the display failed. The message lists every \
createVirtualDisplay signature it tried and what each returned — that list is the \
finding, and it is version-specific. Check that the helper ran as uid 2000 above." ;;
    *"FAIL launch"*)
        die "the display exists but nothing would start on it. Check \
'adb shell am start --display <id> -n <component>' by hand, and that the target \
package is installed." ;;
    *"FAIL frame"*)
        die "the display rendered nothing. 'adb shell dumpsys display' should list \
a DisplayDevice named syscity-agent; if it is there but blank, the app never \
reached the display." ;;
    *"must run as shell"*)
        die "the helper did not get shell identity — it was not started through \
'adb shell', or adbd is not reachable for this user." ;;
    *)
        die "the self-test did not report success (exit $RC); output above" ;;
esac
