import { test, expect } from "./fixtures/app.mjs";
import { plainText } from "./fixtures/scripts.mjs";

/**
 * The ordinary case: send a message, watch the answer stream in, and end with
 * a finished bubble instead of a live one.
 *
 * This is the regression test for the two bugs that motivated the suite — the
 * hook-order error blanked the list with perfectly correct events behind it,
 * and the elapsed time used to be an absolute timestamp.
 */
test.describe("plain chat turn", () => {
  test("streams an answer and finishes with a duration", async ({ ui }) => {
    const app = await ui.launch(plainText("alpha beta gamma delta epsilon"));

    // Before sending: empty session, nothing running.
    await expect(app.page.getByTestId("message-skeleton")).toHaveCount(0);
    await expect(app.page.getByTestId("composer-stop")).toHaveCount(0);

    await app.send("say something");

    // Mid-turn: a live bubble with the status bar, and Stop in place of Send.
    const live = app.lastAssistant.getByTestId("live-status");
    await expect(live).toBeVisible();
    await expect(app.page.getByTestId("composer-stop")).toBeVisible();
    await expect(app.page.getByTestId("message-skeleton")).toBeVisible();

    // The streamed text lands in that same bubble.
    await expect(app.lastAssistant).toContainText("alpha");

    // Once the turn ends, the live bar and Stop go away and the bubble gets
    // its finished footer. The old bug stored `Date.now()` as the duration,
    // which rendered a 56-year turn.
    await expect(live).toHaveCount(0);
    await expect(app.lastAssistant).toContainText("epsilon");
    await expect(app.lastAssistant.getByTestId("turn-duration")).toBeVisible();
    await expect(app.lastAssistant.getByTestId("turn-duration")).not.toContainText("y");
    await expect(app.page.getByTestId("composer-stop")).toHaveCount(0);
    await expect(app.page.getByTestId("composer-send")).toBeVisible();
  });

  test("keeps the user message in the transcript", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"));
    await app.send("remember this sentence");

    await expect(
      app.page.getByTestId("message-user").filter({ hasText: "remember this sentence" }),
    ).toBeVisible();
    // The answer is a *new* bubble, not a rewrite of the question.
    await expect(app.page.getByTestId("message-assistant")).toHaveCount(1);
  });
});
