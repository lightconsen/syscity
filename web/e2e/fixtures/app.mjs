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
  ui: async ({ page }, use, testInfo) => {
    const apps = [];

    const launch = async (script = plainText(), gatewayOpts = {}) => {
      // `cloudUser` answers the cloud identity probe a submitted token
      // triggers; it is not a gateway option, so it is split out here.
      const { cloudUser, ...gwOpts } = gatewayOpts;
      const provider = await startFakeProvider({ script, user: cloudUser ?? null });
      const gateway = await startGateway({
        providerBaseUrl: provider.url,
        ...gwOpts,
      });
      apps.push({ provider, gateway });
      await page.goto(gateway.url);
      // The chat composer is the proof that the gates (model configured,
      // onboarding done) were passed and the socket connected.
      await page.getByTestId("composer-input").waitFor({ timeout: 30_000 });
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
