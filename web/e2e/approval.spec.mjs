import { test, expect } from "./fixtures/app.mjs";
import { shellApproval } from "./fixtures/scripts.mjs";

/**
 * The approval dialog is the one screen where the app stops the agent and
 * asks a person. What it must get right:
 *
 *  - it appears, says which tool and how risky the call is;
 *  - approving lets the tool run and the turn finish;
 *  - denying stops the call but still lets the turn conclude;
 *  - closing (× or Escape) is *not* an answer: the request stays pending, the
 *    turn stays parked, and the sidebar keeps counting it.
 *
 * `shell` declares `requires_approval`, so under the default `[permissions]`
 * mode every shell call asks — the script does not need a dangerous command.
 */
test.describe("approval dialog", () => {
  test("approving lets the turn finish", async ({ ui }) => {
    const app = await ui.launch(shellApproval());

    await app.send("run the probe");
    await expect(app.page.getByTestId("approval-dialog")).toBeVisible();

    // The dialog identifies the call: which tool, and how serious it is.
    await expect(app.page.getByTestId("approval-risk")).toHaveText("High");
    await expect(app.page.getByTestId("approval-dialog")).toContainText("shell");

    // While the person decides, the agent is parked, not working — the live
    // bar says so instead of looking hung.
    await expect(app.page.getByTestId("live-status")).toHaveAttribute(
      "data-status",
      "awaiting_approval",
    );

    await app.page.getByTestId("approval-approve").click();

    // The dialog goes away and the queued approval stops counting — in both
    // places it is counted.
    await expect(app.page.getByTestId("approval-dialog")).toHaveCount(0);
    await expect(app.page.getByTestId("approval-badge")).toHaveCount(0);
    await expect(app.page.getByTestId("titlebar-approval-badge")).toHaveCount(0);

    // The tool ran and the turn concluded with the model's follow-up.
    await expect(app.lastAssistant).toContainText("the command was decided");
    await expect(app.page.getByTestId("composer-stop")).toHaveCount(0);

    // A fresh turn's internals are open, so the finished call is visible
    // without another click.
    await expect(app.lastAssistant.getByTestId("tool-call")).toHaveAttribute(
      "data-status",
      "done",
    );
  });

  test("denying stops the call and still finishes the turn", async ({ ui }) => {
    const app = await ui.launch(shellApproval());

    await app.send("run the probe");
    await expect(app.page.getByTestId("approval-dialog")).toBeVisible();

    // The deny path takes a reason; the plain Deny button only appears after
    // the input is opened.
    await app.page.getByTestId("approval-deny-dots").click();
    await app.page.getByTestId("approval-deny").click();

    await expect(app.page.getByTestId("approval-dialog")).toHaveCount(0);
    await expect(app.lastAssistant).toContainText("the command was decided");
    await expect(app.page.getByTestId("composer-stop")).toHaveCount(0);

    // The call is in the transcript as a failure, not as a silent skip. The
    // agent loop reports a denied call as "Tool execution failed: …", which is
    // what the card's error state is derived from.
    await expect(app.lastAssistant.getByTestId("tool-call")).toHaveAttribute(
      "data-status",
      "error",
    );
  });

  for (const how of ["the close button", "Escape"]) {
    test(`${how} defers without answering`, async ({ ui }) => {
      const app = await ui.launch(shellApproval());

      await app.send("run the probe");
      await expect(app.page.getByTestId("approval-dialog")).toBeVisible();

      if (how === "Escape") {
        await app.page.keyboard.press("Escape");
      } else {
        await app.page.getByTestId("approval-close").click();
      }

      // Dismissed, not decided.
      await expect(app.page.getByTestId("approval-dialog")).toHaveCount(0);

      // The gateway still has the request: the badge counts it, and the turn
      // is still parked on it rather than finished or hung.
      await expect(app.page.getByTestId("approval-badge")).toHaveAttribute("data-count", "1");
      // The toolbar one is the copy that survives a collapsed sidebar, a
      // closed phone drawer and the full-screen pages.
      await expect(app.page.getByTestId("titlebar-approval-badge")).toHaveAttribute(
        "data-count",
        "1",
      );
      await expect(app.page.getByTestId("live-status")).toHaveAttribute(
        "data-status",
        "awaiting_approval",
      );
      await expect(app.page.getByTestId("composer-stop")).toBeVisible();
      await expect(app.page.getByTestId("turn-duration")).toHaveCount(0);

      // The badge brings the question back — the toolbar one, from anywhere.
      await app.page.getByTestId("titlebar-approval-badge").click();
      await expect(app.page.getByTestId("approval-dialog")).toBeVisible();

      // Answering it now still works, which proves the deferred request was
      // left in a decidable state rather than abandoned.
      await app.page.getByTestId("approval-approve").click();
      await expect(app.lastAssistant).toContainText("the command was decided");
    });
  }
});
