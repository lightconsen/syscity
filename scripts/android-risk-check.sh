#!/usr/bin/env bash
#
# Check the irreversible-action gate on a real phone.
#
# What a unit test cannot show: that a real app's tree names the button what the
# classifier expects, that the question really reaches a client, and that a
# refusal really stops the tap from happening. The page is served from this
# machine and opened in Chrome on the device, so nothing outside the test is
# touched:
#
#   <button>发送</button> whose click changes the text below it.
#
# Sequence, each step a claim:
#   1. the button is on screen and reads "发送"
#   2. asking to tap it (with a human present) produces `ask.required`
#   3. answering "Refuse" leaves the page saying "nothing happened"
#   4. asking again and answering "Allow" changes the page
#   5. android_query finds the button the tree names
#   6. android_wait sees the screen settle, sees a label that is there, and
#      says plainly that one which is not there did not appear
#   7. a second send in the same app is paced — the tool waits
#
# The answers go out on a second WebSocket connection: the gateway services
# each connection's frames in order, so a connection blocked inside a tool call
# cannot also answer the question that tool is asking.
#
# Prerequisites: adb, node, a device with Chrome, and a built APK. The gateway
# inside the app grants a local `write` scope, which `debug.tool.invoke` needs.
#
# Usage:
#   ./scripts/android-risk-check.sh               # use the installed app
#   ./scripts/android-risk-check.sh --install     # install the last APK first
#   ./scripts/android-risk-check.sh --device emulator-5554

set -euo pipefail

RED=$'\033[0;31m'
GREEN=$'\033[0;32m'
BLUE=$'\033[0;34m'
RESET=$'\033[0m'

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
PACKAGE="net.syscity.desktop"
APK_DIR="$REPO_ROOT/desktop/gen/android/app/build/outputs/apk"
PORT="${SYSCITY_PORT:-18080}"
PAGE_PORT=8765
DEVICE="${ANDROID_SERIAL:-}"
INSTALL=false

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
command -v node >/dev/null 2>&1 || die "node is not on PATH"
command -v python3 >/dev/null 2>&1 || die "python3 is not on PATH (it serves the test page)"

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

# ── 1. the page ────────────────────────────────────────────────────────────
PAGE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/syscity-risk-check.XXXXXX")
trap 'rm -rf "$PAGE_DIR"; kill "${SERVER_PID:-}" 2>/dev/null || true' EXIT

cat > "$PAGE_DIR/index.html" <<'HTML'
<!doctype html><meta charset="utf-8">
<meta name=viewport content="width=device-width,initial-scale=1">
<title>risk-check</title>
<body style="font:24px sans-serif;margin:2rem">
<p id=done>nothing happened</p>
<button id=send style="font-size:28px;padding:1.5rem 3rem"
        onclick="document.getElementById('done').textContent='it happened'">发送</button>
</body>
HTML

( cd "$PAGE_DIR" && exec python3 -m http.server "$PAGE_PORT" --bind 127.0.0.1 >/dev/null 2>&1 ) &
SERVER_PID=$!
sleep 1
curl -sf -o /dev/null "http://127.0.0.1:$PAGE_PORT/" || die "the test page did not start"

# `adb reverse` is the phone reaching this machine, which is how the page is
# served without putting anything on a network.
adb_cmd reverse "tcp:$PAGE_PORT" "tcp:$PAGE_PORT" >/dev/null
ok "page served and reachable from the device"

adb_cmd shell am force-stop com.android.chrome >/dev/null 2>&1 || true
adb_cmd shell am start -a android.intent.action.VIEW \
    -d "http://127.0.0.1:$PAGE_PORT/" -p com.android.chrome >/dev/null 2>&1
sleep 6
ok "Chrome opened the page"

FORWARDED=$(adb_cmd forward --list 2>/dev/null | grep -c "tcp:$PORT" || true)
if [ "${FORWARDED:-0}" -eq 0 ]; then
    adb_cmd forward "tcp:$PORT" "tcp:$PORT" >/dev/null
fi
TOKEN=$(adb_cmd shell run-as "$PACKAGE" cat "files/syscity/data/gateway_token" 2>/dev/null |
    tr -d '\r')
[ -n "$TOKEN" ] || die "no gateway token on the device — is the app installed and started once?"
ok "gateway token read"

# ── 2. the sequence, through the gateway ───────────────────────────────────
log "driving the gate over the gateway's WebSocket"
set +e
RESULT=$(SYSCITY_TOKEN="$TOKEN" SYSCITY_PORT="$PORT" node --input-type=module -e '
const port = process.env.SYSCITY_PORT;
const token = process.env.SYSCITY_TOKEN;
const base = `http://127.0.0.1:${port}`;

// Two connections, the way a real client works. The gateway services the
// frames of one connection in order, so the connection blocked inside a
// `debug.tool.invoke` is not reading anything — an answer sent on it is only
// seen after the 60 s question has already timed out and been refused. The
// answer therefore comes from a second connection, exactly as another UI would.
const open = async (client) => {
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
  const events = [];
  ws.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    // An event frame is `{type: "event", event: "ask.required", payload}` and
    // a response is matched by `id` — the shape is easy to get wrong and the
    // failure is silence.
    if (message.event) events.push({ ...message, answered: false });
    const finish = message.id ? waiting.get(message.id) : undefined;
    if (finish) { waiting.delete(message.id); finish(message); }
  });
  const request = (id, method, params, timeoutMs = 90000) => new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      waiting.delete(id);
      reject(new Error(`\`${method}\` (${id}) did not answer within ${timeoutMs} ms`));
    }, timeoutMs);
    waiting.set(id, (answer) => { clearTimeout(timer); resolve(answer); });
    ws.send(JSON.stringify({ type: "req", id, method, params }));
  });

  await request(`${client}-connect`, "connect", {
    protocol_version: 1, client: { id: client },
  });
  // Events are routed by session; an empty subscription list is "everything".
  // The answerer is not the client that started the turn, so it has no session
  // of its own to subscribe to.
  await request(`${client}-subscribe`, "subscribe_all", {});
  return { ws, request, events };
};

const driver = await open("android-risk-check");
const answerer = await open("android-risk-answerer");
const tool = async (id, name, args) => {
  const answer = await driver.request(id, "debug.tool.invoke", { tool: name, arguments: args });
  return answer.payload ?? {};
};
const say = (step, text) => console.log(`${step}: ${text}`);
const stop = (reason) => {
  console.log(`FAIL ${reason}`);
  driver.ws.close();
  answerer.ws.close();
  process.exit(1);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Where is the button? The tree is the same one the tool will classify.
const tree = await tool("tree", "android_ui_tree", {});
if (tree.error) stop(`tree: ${tree.output}`);
const elements = tree.data?.elements ?? [];
const button = elements.find((e) => (e.text ?? "").includes("发送"));
if (!button) {
  stop("the page has no 发送 button on screen — is Chrome showing the test page? " +
       `texts: ${elements.map((e) => e.text).filter(Boolean).slice(0, 12).join(" | ")}`);
}
say("button", `"${button.text}" at (${button.center[0]}, ${button.center[1]})`);

const done = async () => {
  const read = await tool(`read-${Date.now()}`, "android_ui_tree", {});
  return (read.data?.elements ?? []).map((e) => e.text ?? "").join(" ");
};
if (!(await done()).includes("nothing happened")) stop("the page does not say \"nothing happened\"");

// 1. Ask to tap it, and refuse. The tap must not happen.
//
// The question carries the options the gate sent, so which button to press is
// read off the question rather than assumed: two answerers both waiting for
// "the next question" is how the Allow answer once went out as a Refuse. The
// answer goes out on the answerer connection, which is free to read while the
// driver is blocked inside the tool call.
const answerWith = async (label, timeoutMs = 30000) => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const question = answerer.events.find(
      (e) => !e.answered && e.event === "ask.required"
        && (e.payload?.options ?? []).includes(label),
    );
    if (question) {
      question.answered = true;
      const reply = await answerer.request(`answer-${label}-${Date.now()}`, "ask.respond", {
        ask_id: question.payload.ask_id, response: label,
      });
      if (!reply.ok) stop(`ask.respond was refused: ${JSON.stringify(reply.error)}`);
      return question.payload;
    }
    await sleep(100);
  }
  return null;
};

const refuseAnswer = answerWith("Refuse");
const refused = await tool("tap-refused", "android_input", {
  action: "tap", x: button.center[0], y: button.center[1], verify: true,
});
const asked = await refuseAnswer;
if (!asked) stop("no question arrived for a 发送 tap — the gate did not fire");
say("asked", asked.question);
// The gate refuses the whole call: the message is in `error`, the tool ran
// nothing. A success here would mean the tap went through.
const refusalText = `${refused.error ?? ""} ${refused.output ?? ""}`;
if (refused.error === undefined || !refusalText.includes("refused")) {
  stop(`the refusal did not stop the tap: ${JSON.stringify(refused)}`);
}
say("refused", "the tap was refused");
if ((await done()).includes("it happened")) stop("the page changed despite the refusal");
say("unchanged", "the page still says nothing happened");

// 2. Ask again, allow, and see it happen. This also paces the next send.
const allowAnswer = answerWith("Allow");
const allowed = await tool("tap-allowed", "android_input", {
  action: "tap", x: button.center[0], y: button.center[1], verify: true,
});
if (!(await allowAnswer)) stop("the second tap was not put to a human");
if (allowed.error) stop(`the allowed tap failed: ${allowed.output}`);
if (!String(allowed.data?.risk ?? "").includes("send")) {
  stop(`the allowed tap did not report a risk: ${JSON.stringify(allowed.data)}`);
}
if (allowed.data?.risk_approved !== true) {
  stop("the allowed tap did not record that it was approved");
}
const afterAllow = await done();
if (!afterAllow.includes("it happened")) {
  stop(`the allowed tap did not take effect; the page says: ${afterAllow.slice(0, 200)}`);
}
say("allowed", "the page now says it happened");

// 3. The two cheap reads find the same things the tree does, one line each.
const queried = await tool("query", "android_query", { text: "发送" });
if (queried.error) stop(`android_query failed: ${queried.error}`);
if (!(queried.data?.total >= 1)) {
  stop(`android_query found nothing for 发送: ${JSON.stringify(queried.data)}`);
}
say("query", `${queried.data.total} match(es), first at ${JSON.stringify(queried.data.matches[0]?.center)}`);

const settled = await tool("wait-stable", "android_wait", { condition: "stable", timeout_ms: 3000 });
if (settled.error) stop(`android_wait stable failed: ${settled.error}`);
if (settled.data?.met !== true) {
  stop(`the screen never settled: ${JSON.stringify(settled.data)}`);
}
say("wait-stable", `${settled.data.waited_ms} ms, ${settled.data.reads} read(s)`);

const appeared = await tool("wait-appear", "android_wait", {
  condition: "appear", text: "it happened", timeout_ms: 2000,
});
if (appeared.data?.met !== true) {
  stop(`android_wait appear did not see a label already on screen: ${JSON.stringify(appeared.data)}`);
}
say("wait-appear", `found after ${appeared.data.waited_ms} ms`);

const missing = await tool("wait-timeout", "android_wait", {
  condition: "appear", text: "no such label anywhere", timeout_ms: 600,
});
if (missing.data?.met !== false) {
  stop(`a wait for something absent reported it appeared: ${JSON.stringify(missing.data)}`);
}
say("wait-timeout", `correctly did not find it (${missing.data.waited_ms} ms)`);

// 4. A second send in the same app is paced. The tool waits, and says so.
//
// It is also a send, so it is asked about again; it is allowed. The pace is
// the point of this step: the previous send finished moments ago, so the gap
// it bought is still owed and the tool sleeps it out before replying.
const current = (await tool("tree-again", "android_ui_tree", {})).data?.elements ?? [];
const again = current.find((e) => (e.text ?? "").includes("发送"));
if (!again) stop("the 发送 button left the screen before the paced tap");
const pacedAnswer = answerWith("Allow");
const paced = await tool("tap-paced", "android_input", {
  action: "tap", x: again.center[0], y: again.center[1], verify: true,
});
if (!(await pacedAnswer)) stop("the paced tap was not put to a human");
if (paced.error) stop(`the paced tap failed: ${paced.error}`);
if (!String(paced.data?.paced ?? "").includes("Waited")) {
  stop(`the second send was not paced: ${JSON.stringify(paced.data)}`);
}
say("paced", paced.data.paced);

console.log("CHECK OK");
driver.ws.close();
answerer.ws.close();
process.exit(0);
' 2>&1)
CLIENT_RC=$?
set -e
printf '%s\n' "$RESULT"

if [ "$(printf '%s' "$RESULT" | tail -1)" = "CHECK OK" ]; then
    echo ""
    ok "the irreversible gate works on this device"
    exit 0
fi
if ! printf '%s' "$RESULT" | grep -q '^FAIL '; then
    die "the gateway client stopped early (exit $CLIENT_RC) without naming a step; output above"
fi
exit 1
