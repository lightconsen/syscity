import { test as base, expect } from "@playwright/test";
import { startFakeProvider } from "./fake-provider.mjs";
import { startGateway } from "./gateway.mjs";
import { plainText } from "./scripts.mjs";

/**
 * Per-test app: a scripted provider, a gateway pointed at it, and the page
 * already past the first-launch gates.
 *
 * Override the provider script by passing it to `launch` — the default is a
 * plain text answer, which is what a spec that only cares about the composer
 * needs.
 */
export class App {
  constructor({ provider, gateway, page }) {
    this.provider = provider;
    this.gateway = gateway;
    this.page = page;
  }

  /** The composer's text input. */
  get input() {
    return this.page.getByTestId("composer-input");
  }

  /** Send a message and wait for the user bubble to appear. */
  async send(text) {
    await this.input.fill(text);
    await this.input.press("Enter");
    await this.page.getByTestId("message-user").filter({ hasText: text }).waitFor();
  }

  /** The newest assistant bubble. */
  get lastAssistant() {
    return this.page.getByTestId("message-assistant").last();
  }

  /** Start a fresh session ("New Session" in the sidebar). */
  async newSession() {
    await this.page.getByRole("button", { name: /new session/i }).first().click();
  }

  /** The session row in the sidebar whose label contains `text`. */
  sessionRow(text) {
    return this.page.getByTestId("session-row").filter({ hasText: text }).first();
  }
}

/**
 * `ui` — a running app for the current test.
 *
 * `ui.launch(script)` starts everything; the fixture closes the gateway and
 * provider whether the test passed or not.
 */
export const test = base.extend({
  /**
   * A mock of the Tauri shell, installed *before* the page's own scripts run.
   *
   * The first-launch connection entry points exist only in the desktop
   * webview (`__TAURI__` in `window`), so a plain browser page cannot reach
   * them. This puts the marker in place and stubs exactly the three commands
   * the connection form calls, recording each `save_connection` payload so a
   * spec can assert what would have been written to `client.toml`.
   *
   * `invoke("relaunch_app")` deliberately never settles: the real backend
   * replaces the process and never answers, and a spec that saw it resolve
   * would be testing a different contract from the one shipped.
   */
  tauriShell: async ({ page }, use) => {
    const calls = [];
    await page.addInitScript(() => {
      const listeners = [];
      window.__TAURI__ = {
        core: { invoke: (...a) => window.__TAURI_INTERNALS__.invoke(...a) },
        event: {
          listen: async (event, handler) => {
            listeners.push({ event, handler });
            return () => {};
          },
        },
      };
      window.__TAURI_INTERNALS__ = {
        transformCallback: () => 1,
        convertFileSrc: (p) => p,
        invoke: async (cmd, args) => {
          if (cmd === "get_api_url") return window.__E2E_API_URL;
          if (cmd === "get_gateway_token") return null;
          if (cmd === "get_platform") return "macos";
          if (cmd === "get_connection") {
            return { mode: "local", host: "127.0.0.1", port: 18080, tls: false, token: null };
          }
          if (cmd === "save_connection") {
            // The refusal lives in Rust (`ConnectionConfig::validate`) and runs
            // *before* the write, so the mock has to match that order as well
            // as the messages — a mock that recorded first would let a spec
            // believe a refused config reached the disk. The strings are the
            // backend's own, verbatim.
            const c = args.config ?? {};
            if (c.mode === "remote") {
              if (!String(c.host ?? "").trim()) {
                throw "Remote mode needs a host — enter the gateway's address";
              }
              if (String(c.host).includes("://")) {
                throw (
                  "Enter the host on its own (no http:// or https://) and use TLS for " +
                  "https / wss"
                );
              }
              if (!c.port) throw "Remote mode needs a port between 1 and 65535";
            }
            const w = window;
            (w.__E2E_SAVED ??= []).push(c);
            return null;
          }
          if (cmd === "relaunch_app") return new Promise(() => {});
          throw new Error(`unexpected Tauri command in a UI test: ${cmd}`);
        },
      };
    });
    /** The connection configs the page asked the shell to save. */
    const saved = () => page.evaluate(() => window.__E2E_SAVED ?? []);
    const shell = { calls, saved };
    await use(shell);
  },

  ui: async ({ page }, use, testInfo) => {
    const apps = [];

    const launch = async (script = plainText(), options = {}) => {
      // `cloudUser` answers the cloud identity probe a submitted token
      // triggers; it is not a gateway option, so it is split out here. The
      // two UI-test knobs live here as well:
      //   `waitForChat` — a spec that expects to land on a launch gate must
      //   not wait for a composer that will never appear.
      //   `apiUrl` — what the mocked shell answers to `get_api_url`, for the
      //   "gateway is unreachable" case.
      const { cloudUser, waitForChat = true, apiUrl, ...gwOpts } = options;
      const provider = await startFakeProvider({ script, user: cloudUser ?? null });
      const gateway = await startGateway({
        providerBaseUrl: provider.url,
        ...gwOpts,
      });
      apps.push({ provider, gateway });
      // The mocked Tauri shell (see the `tauriShell` fixture) resolves the
      // gateway base from this; a plain browser ignores it.
      await page.addInitScript((u) => {
        window.__E2E_API_URL = u;
      }, apiUrl ?? gateway.url);
      await page.goto(gateway.url);
      if (waitForChat) {
        // The chat composer is the proof that the gates (model configured,
        // onboarding done) were passed and the socket connected.
        await page.getByTestId("composer-input").waitFor({ timeout: 30_000 });
      }
      return new App({ provider, gateway, page });
    };

    const ui = { launch, page, testInfo };

    await use(ui);

    // Always tear down, even on a failed assertion: a gateway left running
    // holds a port and an index lock for the next test.
    for (const { provider, gateway } of apps) {
      await gateway.stop().catch(() => {});
      await provider.close().catch(() => {});
    }
  },
});

export { expect };
