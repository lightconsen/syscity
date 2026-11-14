import { test, expect } from "./fixtures/app.mjs";
import { plainText } from "./fixtures/scripts.mjs";

/**
 * The Update section of Settings.
 *
 * It answered "Online updates are disabled in the gateway configuration." and
 * greyed out both buttons on a gateway whose `[update].enabled` was true: the
 * WS handler read only the release-check cache, which is empty until a check
 * runs. `enabled` is a configuration fact, not a property of a successful
 * check, so the section must offer the button even with no network.
 *
 * These tests run at 127.0.0.1, where the test harness blocks nothing — a
 * failed GitHub check is still exercised: the button stays enabled either way.
 */
test.describe("update section", () => {
  async function openUpdateSection(app) {
    await app.page.getByRole("button", { name: "Settings" }).click();
    // Settings is a full page, not a dialog; General is the default tab and
    // carries the Update section, whose first row is the current version.
    const settings = app.page.locator("main");
    await settings
      .getByRole("heading", { name: "Settings" })
      .waitFor({ timeout: 15_000 });
    await settings.getByText("Current Version").waitFor({ timeout: 15_000 });
    return settings;
  }

  test("a configured gateway offers the update, not a disabled note", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"));
    const settings = await openUpdateSection(app);

    // The note is the bug's visible symptom; it must not be there.
    await expect(
      settings.getByText(/online updates are disabled/i),
    ).toHaveCount(0);

    // Both buttons are gated on `enabled`. A plain browser gets "Update Now"
    // (the daemon replaces its binary); the desktop webview gets "Check &
    // Update" (the Tauri updater). Assert on whatever this surface renders.
    const button = settings.getByRole("button", { name: /update/i }).first();
    await expect(button).toBeVisible({ timeout: 15_000 });
    await expect(button).toBeEnabled({ timeout: 15_000 });
  });

  test("a disabled config still explains itself", async ({ ui }) => {
    // The other half of the contract: `enabled` must come from the config,
    // so turning it off has to produce the note. Without this, a handler
    // that always answered `enabled: true` would pass the test above.
    const app = await ui.launch(plainText("ok"), {
      extraConfig: "[update]\nenabled = false\n",
    });
    const settings = await openUpdateSection(app);
    await expect(
      settings.getByText(/online updates are disabled/i),
    ).toBeVisible({ timeout: 15_000 });
  });

  test("the section reports the running version", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"));
    const settings = await openUpdateSection(app);
    // `v` + a semver, next to the "Current Version" row.
    await expect(settings.getByText(/^v\d+\.\d+\.\d+/)).toBeVisible({ timeout: 15_000 });
  });
});
