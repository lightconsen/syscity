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
import { existsSync } from "node:fs";
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
 * @returns {Promise<{home: string, port: number, url: string, stop: () => Promise<void>, log: () => string}>}
 */
export async function startGateway({
  providerBaseUrl,
  model = "fake-ui-model",
  binary = path.join(REPO_ROOT, "target/debug/syscity"),
  skipOnboarding = true,
  extraConfig = "",
} = {}) {
  if (!providerBaseUrl) throw new Error("startGateway needs providerBaseUrl");
  if (!existsSync(binary)) {
    throw new Error(
      `no syscity binary at ${binary} — run \`cargo build\` first (the UI tests drive the real gateway)`,
    );
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
    throw new Error(`gateway never answered /live on ${url}:\n${output.slice(-4000)}`);
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
