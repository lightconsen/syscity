import { test, expect } from "./fixtures/app.mjs";
import { plainText } from "./fixtures/scripts.mjs";

/**
 * The cloud OAuth return page (`/cloud/login/callback`).
 *
 * The browser lands here from the cloud console with the session in the URL
 * fragment. This page used to persist the token and then navigate to `/` no
 * matter how the sign-in was started — so a desktop sign-in (which runs in a
 * real browser tab, with no `window.opener` and no app behind it) left the
 * user staring at a second copy of the app in their browser. The fix gives
 * each entry point its own ending; these tests pin the endings down.
 *
 * The token never reaches the cloud here: with `cloud.enabled = false` the
 * gateway's `cloud.token` refuses the call, which is exactly the failure path
 * too. The page renders either way — that is what these tests are about.
 */
test.describe("cloud login return page", () => {
  test("a desktop sign-in ends on a closable completion page", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"), {
      extraConfig: "\n[cloud]\nenabled = false\n",
    });

    // `flow=desktop` is what the desktop shell's sign-in button sends: the
    // system browser opens this page, so there is no opener to post back to
    // and no app to return to.
    await app.page.goto(`${app.gateway.url}/cloud/login/callback?flow=desktop#token=probe`);

    const result = app.page.getByTestId("cloud-login-result");
    await expect(result).toBeVisible();
    await expect(result).toHaveAttribute("data-state", "failed");
    // The failure is explained on the page — the person has no app window to
    // read a toast in.
    await expect(app.page.getByTestId("cloud-login-title")).toContainText("did not complete");

    // Crucially: the page did not navigate to the app.
    expect(new URL(app.page.url()).pathname).toBe("/cloud/login/callback");

    // The explicit way out exists, because a browser may refuse close().
    await expect(app.page.getByTestId("cloud-login-close")).toBeVisible();
  });

  test("a successful navigation-flow sign-in returns to the app", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"), {
      extraConfig: "\n[cloud]\nenabled = false\n",
    });

    // No `flow` and no opener (a direct visit): the old behavior, kept for
    // the tab that navigated itself here, is to go back to the app.
    await app.page.goto(`${app.gateway.url}/cloud/login/callback`);

    // The app is back: the composer only exists on the chat page.
    await expect(app.page.getByTestId("composer-input")).toBeVisible({ timeout: 30_000 });
    expect(new URL(app.page.url()).pathname).toBe("/");
  });

  test("a successful sign-in says so and offers a way out", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"), {
      // The submitted token is verified against the cloud's /auth/me, which
      // the fake provider stands in for here.
      cloudUser: { id: "u_1", name: "Ada Lovelace", avatar_url: null },
      cloudApiBase: "fake",
    });

    await app.page.goto(`${app.gateway.url}/cloud/login/callback?flow=desktop#token=probe`);

    const result = app.page.getByTestId("cloud-login-result");
    await expect(result).toHaveAttribute("data-state", "done", { timeout: 30_000 });
    await expect(app.page.getByTestId("cloud-login-title")).toContainText("signed in");
    await expect(app.page.getByTestId("cloud-login-close")).toBeVisible();
    // Still on the receipt page — no navigation to the gateway's `/`.
    expect(new URL(app.page.url()).pathname).toBe("/cloud/login/callback");
  });

  test("the callback page renders without the chat app behind it", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"), {
      extraConfig: "\n[cloud]\nenabled = false\n",
    });

    await app.page.goto(`${app.gateway.url}/cloud/login/callback?flow=desktop#token=probe`);

    // No sidebar, no composer: this tab is a sign-in receipt, not a second
    // session of the app.
    await expect(app.page.getByTestId("cloud-login-result")).toBeVisible();
    await expect(app.page.getByTestId("composer-input")).toHaveCount(0);
  });
});
