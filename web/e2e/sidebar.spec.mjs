import { test, expect } from "./fixtures/app.mjs";
import { plainText } from "./fixtures/scripts.mjs";

/**
 * The sidebar's fixed nav.
 *
 * New Session / Extensions / Knowledge Base used to live inside the scrolling
 * session list, so a long history scrolled them off the top and the only way
 * back to them was to scroll the list back — the three entries that are always
 * relevant were the ones a busy sidebar hid first.
 *
 * The test measures the row against the sidebar rather than against a
 * screenshot: after scrolling the session list to the bottom, the nav must not
 * have moved by a pixel.
 */
test.describe("sidebar nav stays put", () => {
  test("scrolling the session list does not move the fixed nav", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"));

    // A history long enough to scroll: every turn becomes a session row.
    const sessions = 14;
    for (let i = 0; i < sessions; i += 1) {
      await app.newSession();
      // Wait for the welcome page to be armed before typing into it: a new
      // session is only created by the first message, and a send that races
      // the toggle lands in the previous conversation.
      await expect(app.page.getByTestId("composer-stop")).toHaveCount(0, { timeout: 15_000 });
      await app.send(`session number ${i}`);
      // The row appears once the gateway has persisted the session.
      await app.sessionRow(`session number ${i}`).waitFor({ timeout: 15_000 });
      // Let the scripted turn finish so the next New Session starts idle.
      await expect(app.page.getByTestId("composer-send")).toBeVisible({ timeout: 15_000 });
    }

    const nav = app.page.getByTestId("sidebar-nav");
    const newSession = nav.getByRole("button", { name: /new session/i });
    await expect(newSession).toBeVisible();

    const list = app.page.getByRole("list").first();
    // Confirm the premise: the list really does overflow.
    const overflow = await list.evaluate((el) => el.scrollHeight - el.clientHeight);
    expect(overflow).toBeGreaterThan(0);

    const before = (await newSession.boundingBox()).y;
    await list.evaluate((el) => el.scrollTo({ top: el.scrollHeight }));
    await app.page.waitForTimeout(200);
    const after = (await newSession.boundingBox()).y;

    expect(after).toBe(before);

    // And the row that *is* allowed to move did move.
    const scrolled = await list.evaluate((el) => el.scrollTop);
    expect(scrolled).toBeGreaterThan(0);
  });
});
