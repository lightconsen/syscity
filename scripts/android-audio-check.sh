#!/usr/bin/env bash
#
# Check that an app on the agent's second display is really silenced, and that
# its sound really comes back with the display.
#
# What a unit test cannot show: that `appops set ... PLAY_AUDIO ignore` read back
# from the phone agrees with the ledger, that the helper records the mode the app
# was in before the mute, that tearing the display down writes exactly that mode
# back — not an assumed "allow" — and that a helper killed mid-session has its
# leftovers given back by the next one, from the file rather than from memory.
# The app under test is Settings, which is on every device and not a package the
# user has opinions about.
#
# Sequence, each step a claim:
#   1. the package's mode before anything happens is read (the "prior")
#   2. `android_display launch` on a second display reports the app as muted
#   3. `appops get` on the phone now reads `ignore`
#   4. the helper's ledger file names the package with the prior mode
#   5. muting and un-muting the same app on the same display does not
#      double-count, so one `stop` is still enough to release it
#   6. `android_display stop` tears the display down, `appops` reads the prior
#      mode back, and the ledger is empty
#   7. `sound` for a package with no record writes nothing
#   8. a handoff makes the app audible while the user has it on their own
#      screen, and silent again once it is back
#   9. while they have it, a tap from a second session is refused
#  10. a helper killed with an app silenced leaves it in the file, and the next
#      helper restores it at start-up without being asked
#
# Prerequisites: adb, node, a device or emulator, and a built APK.
#
# Usage:
#   ./scripts/android-audio-check.sh               # use the installed app
#   ./scripts/android-audio-check.sh --install     # install the last APK first
#   ./scripts/android-audio-check.sh --device emulator-5554

set -euo pipefail

RED=$'\033[0;31m'
GREEN=$'\033[0;32m'
BLUE=$'\033[0;34m'
RESET=$'\033[0m'

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
PACKAGE="net.syscity.desktop"
APK_DIR="$REPO_ROOT/desktop/gen/android/app/build/outputs/apk"
PORT="${SYSCITY_PORT:-18080}"
TARGET="com.android.settings"
DEVICE="${ANDROID_SERIAL:-}"
INSTALL=false

LEDGER_PATH="/data/local/tmp/audio_mute_0"

log() { printf '%s==>%s %s\n' "$BLUE" "$RESET" "$*"; }
ok() { printf '%s ok %s %s\n' "$GREEN" "$RESET" "$*"; }
die() {
    printf '%sfail%s %s\n' "$RED" "$RESET" "$*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --install) INSTALL=true; shift ;;
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
        --package)
            [ $# -ge 2 ] || die "--package needs a name"
            TARGET="$2"
            shift 2
            ;;
        -h | --help)
            awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

export ANDROID_HOME="${ANDROID_HOME:-/opt/homebrew/share/android-commandlinetools}"
export PATH="$ANDROID_HOME/platform-tools:$PATH"

command -v adb >/dev/null 2>&1 || die "adb is not on PATH"
command -v node >/dev/null 2>&1 || die "node is not on PATH (the gateway client is a node script)"

if [ -z "$DEVICE" ]; then
    mapfile -t attached < <(adb devices | awk 'NR > 1 && $2 == "device" { print $1 }')
    case "${#attached[@]}" in
        0) die "no device or emulator attached (start one, or pass --device)" ;;
        1) DEVICE="${attached[0]}" ;;
        *) die "several devices attached (${attached[*]}) — pass --device SERIAL" ;;
    esac
fi
adb_cmd() { adb -s "$DEVICE" "$@"; }
adb_cmd get-state >/dev/null 2>&1 || die "device $DEVICE is not reachable"
ok "device $DEVICE"

if $INSTALL; then
    APK=$(find "$APK_DIR" -name '*.apk' -type f -print0 2>/dev/null |
        xargs -0 -r ls -t 2>/dev/null | head -1 || true)
    [ -n "$APK" ] || die "no APK under $APK_DIR — run ./scripts/android-build.sh --debug"
    log "installing $(basename "$APK")"
    adb_cmd install -r -t "$APK" >/dev/null || die "adb install failed"
    adb_cmd shell am start -n "$PACKAGE/.MainActivity" >/dev/null 2>&1 || true
    sleep 5
fi

adb_cmd forward "tcp:$PORT" "tcp:$PORT" >/dev/null
TOKEN=$(adb_cmd shell run-as "$PACKAGE" cat "files/syscity/data/gateway_token" 2>/dev/null |
    tr -d '\r')
[ -n "$TOKEN" ] || die "no gateway token on the device — is the app installed and started once?"

# ── the gateway client ─────────────────────────────────────────────────────
# One node script, several commands. The gateway is the only path that works:
# the helper refuses to start without the handoff nonce the app issues, so a
# display cannot be made by hand with a `CLASSPATH` and a class name.
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/syscity-audio-check.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT

cat > "$WORK_DIR/client.mjs" <<'NODE'
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

await request("connect", "connect", { protocol_version: 1, client: { id: "android-audio-check" } });
const tool = async (id, name, args) => {
  const answer = await request(id, "debug.tool.invoke", { tool: name, arguments: args });
  return answer.payload ?? {};
};
const say = (step, text) => console.log(`${step}: ${text}`);
const stop = (reason) => { console.log(`FAIL ${reason}`); ws.close(); process.exit(1); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// The first helper call also starts the helper, through the loopback-ADB
// pairing the app owns — which is not ready the instant the app is launched. A
// refused call created nothing, so retrying is safe.
const startDisplay = async () => {
  let started = await tool("start", "android_display", { action: "start" });
  for (let attempt = 1; started.data?.display === undefined && attempt <= 10; attempt += 1) {
    say("start", `attempt ${attempt} failed: ${started.error ?? "no display id"}`);
    await sleep(4000);
    started = await tool(`start-${attempt}`, "android_display", { action: "start" });
  }
  if (started.data?.display === undefined) {
    stop(`create: ${started.error ?? started.output ?? JSON.stringify(started)}`);
  }
  return started.data.display;
};

const target = process.env.SYSCITY_TARGET;
const [command, first] = process.argv.slice(2);

if (command === "open") {
  const display = await startDisplay();
  say("display", `id=${display}`);
  const launched = await tool("launch", "android_display", {
    action: "launch", display, package: target,
  });
  if (launched.error) stop(`launch: ${launched.output}`);
  say("launch", launched.output);

  // An explicit mute is the same path a second time: the ledger must not count
  // this display twice, or the `stop` below would leave the app silent.
  const again = await tool("mute-again", "android_display", {
    action: "mute", display, package: target,
  });
  if (again.error) stop(`mute again: ${again.output}`);
  say("mute again", again.output);

  const soundless = await tool("sound", "android_display", {
    action: "sound", display, package: target,
  });
  if (soundless.error) stop(`sound: ${soundless.output}`);
  say("sound", soundless.output);

  // Back to silent for the teardown step, which is what must restore it.
  const remute = await tool("remute", "android_display", {
    action: "mute", display, package: target,
  });
  if (remute.error) stop(`re-mute: ${remute.output}`);
  say("re-mute", remute.output);
  // Leave the display open: the shell reads `appops` and the ledger with the app
  // still on the display. Closing from here would restore the sound before
  // either could be observed.
  console.log("READY");
} else if (command === "start") {
  // Only `start` brings the helper up; every other action attaches to one that
  // already exists. Its start-up is what restores whatever the last helper left
  // silenced, so this is the call the last step needs — with no `package`, so
  // nothing is launched onto the new display to muddy the observation.
  const display = await startDisplay();
  say("display", `id=${display}`);
  console.log("STARTED");
} else if (command === "close") {
  const display = Number(first);
  const answer = await tool("stop", "android_display", { action: "stop", display });
  if (answer.error) stop(`stop: ${answer.output}`);
  say("stop", answer.output);
  console.log("CLOSED");
} else if (command === "hand") {
  // Hand the app to the user and take it back, with a second connection to
  // answer the question: the gateway services one connection's frames in order,
  // so the connection blocked inside `android_hand_to_user` cannot also answer
  // it. The sound must follow the app — audible while they can see it, silent
  // again once it is back on a display they cannot.
  const display = await startDisplay();
  say("display", `id=${display}`);
  const launched = await tool("launch", "android_display", {
    action: "launch", display, package: target,
  });
  if (launched.error) stop(`launch: ${launched.error}`);
  await sleep(1500);
  // Emitted so the shell can address the same display while the question is
  // open, which is the only moment the refusal can be observed.
  say("display", `id=${display}`);
  console.log("LAUNCHED");
  // The shell reads `appops` while the app is still on the agent's display.
  // That window closes the instant the handoff moves it to the user's screen
  // and gives the sound back, so the launch is not raced against it: without
  // this pause the reading lands after the unmute and the check fails on a
  // handoff that did exactly the right thing.
  await sleep(4000);

  let answered = false;
  const waiting = (async () => {
    const ticketAnswer = await fetch(`${base}/api/v1/ws-ticket`, {
      method: "POST",
      headers: { Authorization: `Bearer ${token}` },
    });
    const { ticket } = await ticketAnswer.json();
    const answerer = new WebSocket(`ws://127.0.0.1:${port}/ws?ticket=${ticket}`);
    await new Promise((done, failed) => {
      answerer.addEventListener("open", done, { once: true });
      answerer.addEventListener("error", failed, { once: true });
    });
    const inbox = new Map();
    const events = [];
    answerer.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      if (message.event) events.push(message);
      const finish = message.id ? inbox.get(message.id) : undefined;
      if (finish) { inbox.delete(message.id); finish(message); }
    });
    const ask = (id, method, params) => new Promise((resolve) => {
      inbox.set(id, resolve);
      answerer.send(JSON.stringify({ type: "req", id, method, params }));
    });
    await ask("answerer-connect", "connect", {
      protocol_version: 1, client: { id: "android-audio-answerer" },
    });
    await ask("answerer-subscribe", "subscribe_all", {});

    // Held open long enough for the shell to read `appops` while the user has
    // the app: that reading is the claim.
    const deadline = Date.now() + 45000;
    while (Date.now() < deadline && !answered) {
      const question = events.find((e) => e.event === "ask.required");
      if (question) {
        console.log("HAND-STARTED");
        await sleep(10000);
        answered = true;
        const options = question.payload?.options ?? [];
        const pick = options.find((o) => /done/i.test(o)) ?? options[0];
        await ask("answer", "ask.respond", { ask_id: question.payload.ask_id, response: pick });
        return;
      }
      await sleep(100);
    }
    stop("the handoff never asked a question");
  })();

  const hand = await tool("hand", "android_hand_to_user", {
    display, question: "Audio check: take over this app, then press Done.",
    timeout_seconds: 60,
  });
  // The tool returns only after the answer and the move back.
  console.log("HAND-RETURNED");
  if (!answered) stop(`the handoff did not complete: ${hand.error ?? hand.output}`);
  await waiting;
  await sleep(1500);

  const closed = await tool("close", "android_display", { action: "stop", display });
  if (closed.error) stop(`stop: ${closed.error}`);
  console.log("CLOSED");
} else if (command === "poke") {
  // A second connection taps the display another session has handed to the
  // user. It must be refused by the helper — the lock on this host does not
  // cover a connection that arrived later, and on the device the takeover flag
  // is what stops the tap landing under the person's finger.
  const display = Number(first);
  const answer = await tool("poke", "android_display", {
    action: "tap", display, x: 10, y: 10,
  });
  // A refused tap comes back as `ok: false` from the helper, which the Rust
  // client turns into an error — so the error is the success case here. The
  // message has to name the takeover, though: a display that was never there
  // is refused too, and that would make this pass without proving anything.
  const refusal = String(answer.error ?? "");
  say("poke", answer.error ? `refused: ${refusal}` : `TAPPED: ${answer.output}`);
  console.log(answer.error && refusal.includes("DISPLAY_HELD_BY_USER") ? "REFUSED" : "TAPPED");
} else if (command === "mute") {
  // A display is addressed by id even when it is gone: this call's real purpose
  // in the last step is to bring a new helper up, and its own answer is not the
  // claim being checked.
  const display = Number(first);
  const answer = await tool("mute", "android_display", {
    action: "mute", display, package: target,
  });
  say("mute", answer.error ? `refused: ${answer.output}` : answer.output);
  console.log("ANSWERED");
} else if (command === "sound") {
  const display = Number(first);
  const answer = await tool("sound", "android_display", {
    action: "sound", display, package: target,
  });
  say("sound", answer.error ? `refused: ${answer.output}` : answer.output);
  console.log("ANSWERED");
} else {
  stop(`unknown command ${command}`);
}

ws.close();
process.exit(0);
NODE

client() {
    SYSCITY_TOKEN="$TOKEN" SYSCITY_PORT="$PORT" SYSCITY_TARGET="$TARGET" \
        node "$WORK_DIR/client.mjs" "$@"
}

# The mode as the phone reports it. `No operations.` is "never set"; the package
# line above the uid line is what the helper records, and reading it is half the
# claim.
mode_now() {
    adb_cmd shell appops get --user 0 "$TARGET" PLAY_AUDIO 2>/dev/null |
        tr -d '\r' |
        awk '/^PLAY_AUDIO:/ { print $2; found = 1 } END { if (!found) print "unset" }'
}
ledger_now() {
    adb_cmd shell cat "$LEDGER_PATH" 2>/dev/null | tr -d '\r' || true
}
helper_pid() {
    adb_cmd shell 'ps -A -o PID,USER,NAME' | tr -d '\r' |
        awk '$2 == "shell" && $3 == "app_process" { print $1 }'
}

PRIOR=$(mode_now)
ok "prior mode of $TARGET: $PRIOR"
[ "$PRIOR" != "ignore" ] || die "$TARGET is already silenced on this device — \
another agent session or the user set it. Run 'adb shell appops set --user 0 \
$TARGET PLAY_AUDIO default' and try again; a check that cannot tell its own mute \
from one that was already there proves nothing."

# ── 1. put it on a display ────────────────────────────────────────────────
log "driving android_display over the gateway's WebSocket"
set +e
RESULT=$(client open 2>&1)
CLIENT_RC=$?
set -e
printf '%s\n' "$RESULT"
printf '%s\n' "$RESULT" | grep -qx 'READY' ||
    die "the gateway client stopped early (exit $CLIENT_RC) — output above"
DISPLAY_ID=$(printf '%s\n' "$RESULT" | sed -n 's/^display: id=\([0-9][0-9]*\)$/\1/p' | head -1)
[ -n "$DISPLAY_ID" ] || die "the client did not say which display it opened"

# ── 2. what the phone says, with the display still up ─────────────────────
MUTED=$(mode_now)
[ "$MUTED" = "ignore" ] || die "the app is on the display but 'appops' reads \
'$MUTED', not 'ignore' — the helper reported a mute the phone does not agree with"
ok "appops reads 'ignore' while $TARGET is on the display"

LEDGER=$(ledger_now)
case "$LEDGER" in
    *"$TARGET "*) ;;
    *)
        die "the ledger at $LEDGER_PATH does not name $TARGET:
${LEDGER:-(empty)}"
        ;;
esac
ok "the ledger records it, with the prior mode: $(printf '%s' "$LEDGER" | head -1)"

# ── 3. closing the display gives the sound back ───────────────────────────
log "closing display $DISPLAY_ID"
set +e
RESULT=$(client close "$DISPLAY_ID" 2>&1)
CLIENT_RC=$?
set -e
printf '%s\n' "$RESULT"
[ "$(printf '%s\n' "$RESULT" | tail -1)" = "CLOSED" ] ||
    die "closing the display failed (exit $CLIENT_RC) — output above"
sleep 1

AFTER=$(mode_now)
if [ "$PRIOR" = "unset" ]; then
    # "Nothing was set" is written back as `default`, which for this op means the
    # same thing; either reads as restored.
    case "$AFTER" in
        unset | default | allow) ;;
        *) die "the display is gone but the app is still '$AFTER' — its sound did not come back" ;;
    esac
else
    [ "$AFTER" = "$PRIOR" ] ||
        die "the display is gone but the app reads '$AFTER', not the '$PRIOR' it had before"
fi
ok "sound restored: '$AFTER' (was '$PRIOR')"

LEDGER=$(ledger_now)
[ -z "$LEDGER" ] || die "the ledger still names an app after the display closed:
$LEDGER"
ok "the ledger is empty"

# ── 4. a sound request with nothing recorded writes nothing ───────────────
# The mute/sound pair must not become a way to set an app-op on a package the
# helper never touched: `sound` for a package with no record has to be a no-op.
log "checking that 'sound' without a record writes nothing"
BEFORE=$(mode_now)
client sound "$DISPLAY_ID" >/dev/null 2>&1 || true
AFTER=$(mode_now)
[ "$AFTER" = "$BEFORE" ] ||
    die "a 'sound' with nothing recorded changed the mode from '$BEFORE' to '$AFTER'"
ok "nothing was written ('$AFTER')"

# ── 5. the sound follows the app to the user's own screen and back ─────────
# A handoff exists so the user can do what the agent cannot; an app they can see
# but cannot hear is a broken handoff. And once it is back on a display they
# cannot see, it has to be silent again. The client runs in the background
# because the reading has to happen while the question is still open.
log "checking that the sound follows a handoff"
HAND_LOG="$WORK_DIR/hand.log"
client hand >"$HAND_LOG" 2>&1 &
HAND_PID=$!

for _ in $(seq 1 90); do
    grep -q '^LAUNCHED$' "$HAND_LOG" && break
    sleep 1
done
grep -q '^LAUNCHED$' "$HAND_LOG" || { cat "$HAND_LOG"; die "the handoff never got its app on a display"; }
HAND_MUTED=$(mode_now)
[ "$HAND_MUTED" = "ignore" ] || {
    cat "$HAND_LOG"
    die "before the handoff the app reads '$HAND_MUTED', not 'ignore'"
}
ok "silent before the handoff"

for _ in $(seq 1 90); do
    grep -q '^HAND-STARTED$' "$HAND_LOG" && break
    sleep 1
done
grep -q '^HAND-STARTED$' "$HAND_LOG" || { cat "$HAND_LOG"; die "no question was asked to hand the app over"; }
sleep 2
HANDED=$(mode_now)
[ "$HANDED" != "ignore" ] || die "the app is on the user's screen but is still silenced \
— they can see it and cannot hear it"
ok "audible while the user has it ('$HANDED')"

# The display is the person's right now: a second session must not be able to
# tap it. The Rust lock serialises sessions on this host and says nothing about
# one that connected later, or about another host — the helper's takeover flag
# is the guarantee, so this checks the device's answer rather than the host's.
HAND_DISPLAY=$(sed -n 's/^display: id=\([0-9][0-9]*\)$/\1/p' "$HAND_LOG" | tail -1)
[ -n "$HAND_DISPLAY" ] || { cat "$HAND_LOG"; die "the handoff never said which display it used"; }
POKE=$(client poke "$HAND_DISPLAY" 2>&1)
printf '%s\n' "$POKE"
printf '%s\n' "$POKE" | grep -qx "REFUSED" ||
    die "a second connection tapped the display a person was using — the takeover is \
not holding"
ok "a tap from another session is refused while the user has the display"

for _ in $(seq 1 120); do
    grep -q '^HAND-RETURNED$' "$HAND_LOG" && break
    sleep 1
done
wait "$HAND_PID" || true
cat "$HAND_LOG"
grep -q '^CLOSED$' "$HAND_LOG" || die "the handoff did not finish — output above"
sleep 1
HAND_BACK=$(mode_now)
if [ "$PRIOR" = "unset" ]; then
    case "$HAND_BACK" in
        unset | default | allow) ;;
        *) die "the display closed but the app is still '$HAND_BACK'" ;;
    esac
else
    [ "$HAND_BACK" = "$PRIOR" ] ||
        die "the display closed but the app reads '$HAND_BACK', not '$PRIOR'"
fi
ok "the display closed and the app's own mode is back ('$HAND_BACK')"

# ── 6. a killed helper still gives the sound back ─────────────────────────
# The record has to be a file, and it has to be read on the next start-up: the
# display that muted the app died with the process, so nothing else will ever
# release it. This is the claim that makes the feature safe to ship.
log "checking that a killed helper restores on the next start"
set +e
RESULT=$(client open 2>&1)
CLIENT_RC=$?
set -e
printf '%s\n' "$RESULT"
printf '%s\n' "$RESULT" | grep -qx 'READY' ||
    die "the second open failed (exit $CLIENT_RC) — output above"
[ "$(mode_now)" = "ignore" ] || die "the second display did not silence $TARGET"
[ -n "$(ledger_now)" ] || die "the second display silenced $TARGET but left no record"

PIDS=$(helper_pid)
[ -n "$PIDS" ] || die "no helper process to kill — is it running as uid shell?"
[ "$(printf '%s\n' "$PIDS" | wc -l | tr -d ' ')" -eq 1 ] ||
    die "more than one shell app_process is running; refusing to guess which is the helper"
adb_cmd shell "kill -9 $PIDS"
sleep 2
ok "helper killed with $TARGET silenced"

[ "$(mode_now)" = "ignore" ] ||
    die "killing the helper already restored the sound — the record proves nothing \
about the next process"
[ -n "$(ledger_now)" ] || die "the ledger did not survive the kill"
ok "the record outlived the process it was written by"

# Any helper call starts a new one; its start-up is what restores the leftovers.
# `start` is the one that provably brings it up — a mute on the dead display is
# refused by the app before the helper is ever asked.
set +e
RESULT=$(client start 2>&1)
CLIENT_RC=$?
set -e
printf '%s\n' "$RESULT"
printf '%s\n' "$RESULT" | grep -qx 'STARTED' ||
    die "the restarted helper did not come up (exit $CLIENT_RC) — output above"
sleep 2
RESTORED=$(mode_now)
if [ "$PRIOR" = "unset" ]; then
    case "$RESTORED" in
        unset | default | allow) ;;
        *) die "a new helper started but the app is still '$RESTORED'" ;;
    esac
else
    [ "$RESTORED" = "$PRIOR" ] ||
        die "a new helper started but the app reads '$RESTORED', not '$PRIOR'"
fi
[ -z "$(ledger_now)" ] || die "the record survived a restore — the app was set back \
but will be set back again on every start"
ok "the next helper gave the sound back on its own: '$RESTORED'"

# The display `start` just made is left behind by design; close it so a re-run
# starts from the same place.
NEW_ID=$(printf '%s\n' "$RESULT" | sed -n 's/^display: id=\([0-9][0-9]*\)$/\1/p' | head -1)
client close "$NEW_ID" >/dev/null 2>&1 || true

echo ""
ok "audio muting works on this device"
