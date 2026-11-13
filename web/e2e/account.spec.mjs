import { test, expect } from "./fixtures/app.mjs";
import { plainText } from "./fixtures/scripts.mjs";

/**
 * The signed-in profile button.
 *
 * `avatar_url` is a remote image the cloud hands us, and it can fail to load
 * for reasons that have nothing to do with the account: no network (a proxy
 * that allows the API but not the avatar host) or the shell's CSP. When that
 * happened the titlebar rendered the browser's "missing image" glyph — a
 * 24px button with a broken-image icon on it, which reads as an empty button
 * rather than as "signed in as A". The fix falls back to the initial, the
 * same mark a user without an avatar gets.
 */
test.describe("signed-in avatar", () => {
  test("a broken avatar URL falls back to the account initial", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"), {
      // Sign in for real: the fake provider answers /auth/me.
      cloudUser: {
        id: "u_1",
        name: "Ada Lovelace",
        // Deliberately unreachable — the test cares about the failure path.
        avatar_url: "http://127.0.0.1:1/never.png",
      },
      cloudApiBase: "fake",
    });

    await app.page.goto(`${app.gateway.url}/cloud/login/callback?flow=desktop#token=probe`);
    await expect(app.page.getByTestId("cloud-login-result")).toHaveAttribute(
      "data-state",
      "done",
      { timeout: 30_000 },
    );

    // Back in the app: the gateway now has a session token.
    await app.page.goto(app.gateway.url);
    await app.page.getByTestId("composer-input").waitFor({ timeout: 30_000 });

    const account = app.page.getByRole("button", { name: "Account" });
    await expect(account).toBeVisible();

    // No <img> is left in the button — it would be the broken-image glyph.
    await expect(account.locator("img")).toHaveCount(0, { timeout: 15_000 });
    // The initial takes its place, in the same spot.
    await expect(account).toContainText("A");
    await expect(account).toHaveAttribute("title", "Ada Lovelace");
  });
});
