import { test, expect } from "./fixtures/app.mjs";
import { slowText } from "./fixtures/scripts.mjs";

/**
 * A reload in the middle of a turn.
 *
 * This is the regression test for the bug that motivated the run-state work:
 * the gateway kept generating, the events were correct, and the reloaded page
 * showed a stalled conversation with no Stop button — indistinguishable from a
 * hung agent.
 *
 * The state that makes the difference is the gateway's, not the page's: the
 * turn is still running, `sessions.status` says so, and the client adopts it
 * instead of assuming a fresh page means an idle agent.
 */
test.describe("reload mid-turn", () => {
  test("a reloaded page still shows the turn as running", async ({ ui }) => {
    const app = await ui.launch(slowText({ chunkDelayMs: 1200, chunkCount: 14 }));
    const page = app.page;

    await app.send("write something slow");
    await expect(page.getByTestId("live-status")).toBeVisible();
    await expect(page.getByTestId("composer-stop")).toBeVisible();
    await expect(page.getByTestId("message-skeleton")).toBeVisible();

    // Let a couple of chunks land, so the reloaded page has partial text to
    // restore as well as a running turn.
    await expect(app.lastAssistant).toContainText("chunk1");

    await page.reload();
    // Past the gates again: the composer is the sign the socket is up.
    await page.getByTestId("composer-input").waitFor({ timeout: 30_000 });

    // The running turn is visible straight from the gateway's answer — no
    // waiting for the next event to discover it.
    await expect(page.getByTestId("live-status")).toBeVisible();
    await expect(page.getByTestId("composer-stop")).toBeVisible();
    await expect(page.getByTestId("message-skeleton")).toBeVisible();

    // And the sidebar marks the session as running too, so switching away
    // from it would not look idle either.
    await expect(page.getByTestId("session-row").first().getByTestId("session-running")).toBeVisible();

    // The partial answer survived the reload rather than starting blank.
    await expect(page.getByTestId("message-assistant").last()).toContainText("chunk1");

    // When the turn ends, the live view lets go: no live bar, no Stop, a
    // finished bubble with a duration.
    const bubble = page.getByTestId("message-assistant").last();
    await expect(bubble.getByTestId("live-status")).toHaveCount(0, { timeout: 45_000 });
    await expect(page.getByTestId("composer-stop")).toHaveCount(0);
    await expect(bubble).toContainText("chunk14");
    await expect(bubble.getByTestId("turn-duration")).toBeVisible();
    await expect(
      page.getByTestId("session-row").first().getByTestId("session-running"),
    ).toHaveCount(0);
  });
});
