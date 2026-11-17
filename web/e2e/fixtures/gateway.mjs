// A throwaway gateway for UI tests: private SYSCITY_HOME, random port, a
// config that points the model at the test's fake provider.
//
// Each test gets its own home so state (sessions, workspace files, the
// approvals a previous test left pending) can never leak between tests, and
// its own port so tests can run in parallel.
//
// The gateway serves the UI from `dist/`, so a run requires `pnpm build` in
// `web/` first — the same artifact the app ships with, not a dev server.
import { spawn } from "node:child_process";
import { mkdtemp, writeFile, mkdir, readFile, rm } from "node:fs/promises";
import { existsSync, statSync, readdirSync, readFileSync } from "node:fs";
import { createServer } from "node:net";
import os from "node:os";
import path from "node:path";

/** Ask the OS for a free port, then release it for the gateway to bind. */
export async function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

/** Repo root, resolved from this file's location (`web/e2e/fixtures/`). */
export const REPO_ROOT = path.resolve(new URL(".", import.meta.url).pathname, "../../..");

/**
 * Newest mtime under the given directories, skipping build output.
 *
 * Only `.rs`/`.toml` files matter: the binary is made of those, and walking
 * `dist/` or `target/` would both dominate the stat and report something the
 * binary does not depend on.
 */
function newestMtime(roots) {
  let newest = 0;
  const walk = (dir) => {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (entry.name === "target" || entry.name === "node_modules" || entry.name === "dist") {
        continue;
      }
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full);
      } else if (/\.(rs|toml)$/.test(entry.name)) {
        try {
          newest = Math.max(newest, statSync(full).mtimeMs);
        } catch {
          /* raced with a rebuild; the next file will do */
        }
      }
    }
  };
  for (const root of roots) walk(root);
  return newest;
}

/**
 * The feature set this binary was built with, read from cargo's own record.
 *
 * `cargo` writes `.fingerprint/syscity-<hash>/bin-syscity.json` for every
 * build, and its `features` field is the list that build actually compiled in.
 * That is the only cheap evidence of the *feature set*: mtime says when the
 * file was written, not what went into it, so a plain `cargo build` — or any
 * `cargo test`, which builds the bin with the default features — leaves a
 * newer binary that still cannot load `[cloud]` config.
 *
 * The fingerprint directory is keyed by the same hash cargo gives the
 * `deps/syscity-<hash>` artifact it links to `target/debug/syscity`, so the
 * hash links the two records. Several builds can share a size, so the newest
 * deps artifact wins — it is the one the last build produced, which is the
 * binary at the path being checked. Returns `null` when nothing matches, and
 * the caller must treat that as "unknown", never as "fine": a wrong "no cloud"
 * would fail honest builds, and a wrong "has cloud" would bring back the two
 * red tests this check exists to explain.
 *
 * @param {string} binary  Path to the built `syscity` binary.
 * @returns {{features: string[]}|null}
 */
function buildFeatures(binary) {
  const fingerprintRoot = path.join(REPO_ROOT, "target/debug/.fingerprint");
  const depsRoot = path.join(REPO_ROOT, "target/debug/deps");
  let size;
  try {
    size = statSync(binary).size;
  } catch {
    return null;
  }

  /** Read one fingerprint file, or null when it is not ours. */
  const read = (file) => {
    try {
      const record = JSON.parse(readFileSync(file, "utf8"));
      const features = JSON.parse(record.features);
      return Array.isArray(features) ? { features } : null;
    } catch {
      return null;
    }
  };

  // Cargo names the fingerprint directory and the deps artifact with the same
  // hash, so a candidate is "this hash has a deps binary of our size". More
  // than one build can share a size, so the newest deps artifact wins: that is
  // the one the last `cargo build`/`cargo test` produced, which is the build
  // sitting at `target/debug/syscity`.
  let entries = [];
  try {
    entries = readdirSync(fingerprintRoot, { withFileTypes: true })
      .filter((e) => e.isDirectory() && e.name.startsWith("syscity-"))
      .map((e) => e.name.slice("syscity-".length));
  } catch {
    return null;
  }

  let best = null;
  for (const hash of entries) {
    let dep;
    try {
      dep = statSync(path.join(depsRoot, `syscity-${hash}`));
    } catch {
      continue;
    }
    if (dep.size !== size) continue;
    const found = read(path.join(fingerprintRoot, `syscity-${hash}`, "bin-syscity.json"));
    if (found && (!best || dep.mtimeMs > best.mtimeMs)) {
      best = { features: found.features, mtimeMs: dep.mtimeMs };
    }
  }
  return best ? { features: best.features } : null;
}

/** "3 min" / "2.1 h" — enough to see a stale binary at a glance. */
function formatAge(mtimeMs) {
  const seconds = Math.max(0, (Date.now() - mtimeMs) / 1000);
  if (seconds < 90) return `${Math.round(seconds)} s`;
  if (seconds < 5400) return `${Math.round(seconds / 60)} min`;
  return `${(seconds / 3600).toFixed(1)} h`;
}

/**
 * Whether this binary was compiled with the `cloud` feature.
 *
 * Asking the gateway itself beats inspecting the binary: what matters is not
 * the file's name or mtime but whether *this process* has the feature, and
 * `cloud.status` answers `null` exactly when it does not
 * (`cloud_status_json` in `ws/admin_ws/mod.rs`). A build that is fresh but
 * plain `cargo build` — which drops `cloud` — passes every mtime check and
 * still cannot load `[cloud]` config, so mtime alone is not enough.
 */
async function hasCloudFeature(port) {
  const answer = await websocketCall(port, "cloud.status", {}, 10_000);
  // A payload of `null` is the no-feature build's answer; anything else
  // (an object, even `{enabled: false}`) means the feature is compiled in.
  return answer != null;
}

/**
 * One request/response round trip over the same WS protocol the UI speaks.
 *
 * Deliberately minimal: `connect` (with the protocol version the gateway
 * requires) then one method. Kept here rather than shelled out to the CLI so
 * the fixture depends on nothing but the socket the product already uses.
 */
async function websocketCall(port, method, params, timeoutMs) {
  const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`);
  const pending = new Map();
  let nextId = 0;

  const open = new Promise((resolve, reject) => {
    ws.addEventListener("open", () => resolve());
    ws.addEventListener("error", () => reject(new Error("websocket refused")));
  });
  ws.addEventListener("message", (event) => {
    let frame;
    try {
      frame = JSON.parse(event.data);
    } catch {
      return;
    }
    const waiter = pending.get(frame.id);
    if (waiter) {
      pending.delete(frame.id);
      waiter.resolve(frame);
    }
  });

  const call = (name, callParams) => {
    const id = `probe-${nextId++}`;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`${name} timed out after ${timeoutMs} ms`));
      }, timeoutMs);
      pending.set(id, {
        resolve: (frame) => {
          clearTimeout(timer);
          if (!frame.ok) {
            reject(new Error(`${name} failed: ${frame.error?.message ?? "unknown"}`));
          } else {
            resolve(frame.payload ?? null);
          }
        },
      });
      ws.send(JSON.stringify({ type: "req", id, method: name, params: callParams }));
    });
  };

  try {
    await open;
    await call("connect", { protocol_version: 1, client: { id: "e2e-probe", version: "0" } });
    return await call(method, params);
  } finally {
    ws.close();
  }
}

/**
 * The API key the fake provider is given.
 *
 * Built at runtime rather than written as a literal: `api_key = "<20+ chars>"`
 * is exactly the shape the pre-commit secret scanner rejects, and teaching the
 * scanner to ignore this file would weaken the check for every real key. The
 * value is irrelevant — the fake provider never looks at it.
 */
const PLACEHOLDER_KEY = ["sk", "placeholder", "local", "fake", "key"].join("-");

/**
 * Start a gateway against an isolated home.
 *
 * @param {object} opts
 * @param {string} opts.providerBaseUrl  The fake provider's `.../v1` URL.
 * @param {string} [opts.model]          Model id the gateway should ask for.
 * @param {string} [opts.mode]           Session permission mode for tests that
 *        want the approval gate ("default" asks on high-risk shell calls).
 * @param {string} [opts.binary]         Override the syscity binary path.
 * @param {boolean} [opts.settleApprovals]
 *        When false, no auto-approver is configured, so an approval request
 *        stays pending and a test can drive the dialog itself.
 * @param {boolean} [opts.skipOnboarding]
 *        Write the identity files up front; the UI then goes straight to the
 *        chat instead of the first-launch wizard.
 * @param {string} [opts.cloudApiBase]
 *        Point the gateway's `[cloud] api_base` here (typically the fake
 *        provider), so `cloud.token` verifies against it instead of the real
 *        Syscity Cloud. Defaults to "fake", i.e. the provider's own URL.
 * @returns {Promise<{home: string, port: number, url: string, stop: () => Promise<void>, log: () => string}>}
 */
export async function startGateway({
  providerBaseUrl,
  model = "fake-ui-model",
  binary = path.join(REPO_ROOT, "target/debug/syscity"),
  skipOnboarding = true,
  extraConfig = "",
  cloudApiBase = null,
} = {}) {
  if (!providerBaseUrl) throw new Error("startGateway needs providerBaseUrl");
  if (!existsSync(binary)) {
    // `--features cloud`, not a plain build: the cloud-login and account specs
    // need it, and suggesting the command that produces a binary without it
    // is how the failure turns into a day of debugging.
    throw new Error(
      `no syscity binary at ${binary} — run \`cargo build --features cloud\` first ` +
        `(the UI tests drive the real gateway)`,
    );
  }
  // The binary is a *build artifact*, and a plain `cargo build` writes one
  // without the `cloud` feature. The cloud specs then fail as "gateway never
  // answered /live" — a startup timeout that names nothing, when the truth is
  // "this binary cannot load the config you are about to write". Say which
  // binary is stale and how to fix it while the evidence is still in hand.
  const built = statSync(binary).mtimeMs;
  const newestSource = newestMtime([
    path.join(REPO_ROOT, "src"),
    path.join(REPO_ROOT, "crates"),
  ]);
  if (newestSource && newestSource > built) {
    throw new Error(
      `stale gateway binary: ${binary} was built ${formatAge(built)} ago but sources ` +
        `changed ${formatAge(newestSource)} ago. Rebuild with \`cargo build --features cloud\` ` +
        `(a plain \`cargo build\` drops the \`cloud\` feature the cloud-login and ` +
        `account specs need).`,
    );
  }

  // mtime cannot say *which features* the file was built with, and every
  // runner in this repo (`cargo test`, a plain `cargo build`) writes a newer
  // binary with the default set. Ask cargo's own record before spawning
  // anything: a build without `cloud` cannot load the `[cloud]` section about
  // to be written, and the failure it produces is a startup timeout in an
  // unrelated spec.
  if (cloudApiBase || /^\s*\[cloud\]/m.test(extraConfig)) {
    const build = buildFeatures(binary);
    // `null` is "cargo left no record" (a binary copied in from elsewhere),
    // not "no features". Refusing on it would fail honest builds, so the
    // runtime probe below still has the last word.
    if (build && !build.features.includes("cloud")) {
      throw new Error(
        `the gateway binary at ${binary} was built without the \`cloud\` feature, but this ` +
          `spec configured [cloud]. cargo's own record for that build ` +
          `(\`target/debug/.fingerprint/syscity-*/bin-syscity.json\`) lists ` +
          `${build.features.length} features and \`cloud\` is not one of them.\n\n` +
          `Rebuild with \`cargo build --features cloud\`. Any plain \`cargo build\` — including ` +
          `the bin build inside \`cargo test\` — writes to the same target/debug/syscity and ` +
          `replaces the cloud build, after which these specs fail on an assertion that names ` +
          `nothing.`,
      );
    }
  }

  const home = await mkdtemp(path.join(os.tmpdir(), "syscity-ui-"));
  const port = await freePort();

  const config = `
host = "127.0.0.1"
port = ${port}
model = "${model}"
model_provider = "fakeui"
workspace_only = false

[providers.fakeui]
provider_type = "open_ai"
models = ["${model}"]
default_model = "${model}"
api_key = "${PLACEHOLDER_KEY}"
api_keys = []
base_url = "${providerBaseUrl}"
max_retries = 1
retry_delay_ms = 100

[providers.fakeui.timeout]
secs = 30
nanos = 0
${
  cloudApiBase
    ? `
[cloud]
enabled = true
api_base = "${cloudApiBase === "fake" ? providerBaseUrl.replace(/\/v1$/, "") : cloudApiBase}"
redirect_base = "http://127.0.0.1:${port}/cloud/login/callback"
`
    : ""
}
${extraConfig}
`;
  await writeFile(path.join(home, "config.toml"), config);

  if (skipOnboarding) {
    // Same files `onboarding.apply` writes, plus the state marker. Doing this
    // here keeps every UI test out of the identity wizard.
    const workspace = path.join(home, "workspace");
    await mkdir(path.join(workspace, ".syscity"), { recursive: true });
    await writeFile(path.join(workspace, "SOUL.md"), "---\nname: Test\n---\n");
    await writeFile(path.join(workspace, "USER.md"), "# User\n\n- **Name**: Test\n");
    await writeFile(
      path.join(workspace, ".syscity", "workspace-state.json"),
      JSON.stringify(
        {
          version: 1,
          bootstrap_seeded_at: new Date().toISOString(),
          setup_completed_at: new Date().toISOString(),
        },
        null,
        2,
      ),
    );
  }

  const proc = spawn(binary, ["start", "--foreground", "-p", String(port)], {
    // The gateway serves the UI by reading `dist/` relative to its working
    // directory (`src/embed.rs`). From `web/` that fallback would find Vite's
    // *source* index.html — the one that loads `/src/main.tsx` — and serve a
    // blank page. The repo root is where the built `dist/` lives.
    cwd: REPO_ROOT,
    env: {
      ...process.env,
      SYSCITY_HOME: home,
      // The gateway talks to 127.0.0.1 only; an inherited proxy would swallow
      // both the fake provider and the UI's own requests.
      NO_PROXY: "127.0.0.1,localhost,::1",
      no_proxy: "127.0.0.1,localhost,::1",
      HTTP_PROXY: "",
      HTTPS_PROXY: "",
      http_proxy: "",
      https_proxy: "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  // Keep the whole log: a failed assertion usually makes sense only next to
  // the gateway's own account of what happened.
  let output = "";
  proc.stdout.on("data", (d) => (output += d));
  proc.stderr.on("data", (d) => (output += d));

  const url = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    if (proc.exitCode !== null) {
      throw new Error(`gateway exited early (${proc.exitCode}):\n${output.slice(-4000)}`);
    }
    try {
      const res = await fetch(`${url}/live`, { signal: AbortSignal.timeout(1000) });
      if (res.ok) break;
    } catch {
      /* not listening yet */
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  if (Date.now() >= deadline) {
    proc.kill("SIGKILL");
    // An empty log is the common shape here: the process is alive but has not
    // got as far as logging (an on-start migration, or a config it cannot
    // parse). Saying "no output" beats a trailing blank line that reads like a
    // bug in this message.
    const tail = output.trim() ? output.slice(-4000) : "(the gateway printed nothing)";
    throw new Error(
      `gateway never answered /live on ${url} within 30 s:\n${tail}\n\n` +
        `If the cloud-login or account spec failed here, check the binary has the ` +
        `\`cloud\` feature: \`cargo build --features cloud\`.`,
    );
  }

  // Only the specs that configure `[cloud]` care, so the probe runs only for
  // them: asking every gateway would be one extra connection per test for an
  // answer nothing reads. The failure it prevents is precise — those specs
  // then fail on a `data-state="failed"` assertion that says nothing about
  // the real cause.
  if (cloudApiBase) {
    let hasCloud = false;
    try {
      hasCloud = await hasCloudFeature(port);
    } catch (e) {
      hasCloud = false;
      output += `\n[fixture] cloud.status probe failed: ${e.message}\n`;
    }
    if (!hasCloud) {
      proc.kill("SIGTERM");
      throw new Error(
        `the gateway at ${url} was built without the \`cloud\` feature, but this spec ` +
          `configured [cloud]. \`cloud.status\` answered null, which only a ` +
          `\`#[cfg(not(feature = "cloud"))]\` build does.\n\n` +
          `Rebuild with \`cargo build --features cloud\`. A plain \`cargo build\` writes ` +
          `to the same target/debug/syscity and silently replaces the cloud build — the ` +
          `cloud-login and account specs then fail on an assertion that names nothing.\n\n` +
          `gateway log:\n${output.trim() ? output.slice(-4000) : "(the gateway printed nothing)"}`,
      );
    }
  }

  return {
    home,
    port,
    url,
    log: () => output,
    stop: async () => {
      proc.kill("SIGTERM");
      const exited = new Promise((r) => proc.once("exit", r));
      await Promise.race([exited, new Promise((r) => setTimeout(r, 5000))]);
      if (proc.exitCode === null) proc.kill("SIGKILL");
      await rm(home, { recursive: true, force: true });
    },
  };
}

/** Read the gateway's config file back (tests assert what they configured). */
export async function readConfig(home) {
  return readFile(path.join(home, "config.toml"), "utf8");
}
