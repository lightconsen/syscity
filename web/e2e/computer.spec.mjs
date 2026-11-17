// The desktop-run panel: what a computer run did, in order, seen from the UI.
//
// The engine proved its side in `tests/e2e/computer_run_tests.rs`; what this
// spec proves is the part that suite structurally cannot: that a run's steps
// reach the *screen*. The same class of bug bit this UI twice (a hook-order
// error that blanked the message list; a reloaded page showing a running turn
// as stopped), both behind a perfectly correct event stream.
//
// A live run needs a desktop adapter and a model, which is why the on-disk
// path is the one driven here: a checkpoint file is exactly what the gateway
// serves through `computer.runs`, and it is hermetic on any machine.
import path from "node:path";
import { mkdir, writeFile } from "node:fs/promises";
import { expect, test } from "./fixtures/app.mjs";

/**
 * Write a checkpoint into the gateway's own `computer-runs/` directory.
 *
 * `computer.runs` reads exactly this file, so the test drives the real read
 * path without needing a desktop adapter or a model to produce a run.
 */
async function writeCheckpoint(gateway, checkpoint) {
  const dir = path.join(gateway.home, "computer-runs");
  await mkdir(dir, { recursive: true });
  await writeFile(
    path.join(dir, `${checkpoint.run_id}.json`),
    JSON.stringify(checkpoint, null, 2),
  );
}

/** A checkpoint the gateway will read back, shaped as `RunStore` writes it. */
function checkpoint({ runId, goal, sessionId, steps }) {
  return {
    run_id: runId,
    goal,
    session_id: sessionId,
    turn_id: null,
    max_steps: 30,
    settle_delay_ms: 200,
    verify_after_each: true,
    model_calls: steps.length,
    images_attached: 0,
    images_repeated: 0,
    tokens_in: 1200,
    tokens_out: 300,
    updated_at: new Date().toISOString(),
    completed: steps.map((s, i) => ({
      step: i,
      // A shape `DesktopAction` deserialises into; the action's *text* is
      // derived by the gateway, so this only has to be valid.
      action: { key_press: { keys: [s.key] } },
      verification: s.verified ? "verified" : "unverifiable",
      result: s.result,
      sensitive: false,
      rung: s.rung,
      effect: s.effect,
    })),
  };
}

test.describe("desktop run panel", () => {
  test("a run's steps are visible from the conversation that started it", async ({ ui, page }) => {
    const app = await ui.launch();
    // Start a conversation so the page has a session id to match the run to.
    await app.send("open the calculator and add two numbers");

    const sessionId = await page
      .locator('[data-testid="session-row"][data-active="true"]')
      .first()
      .getAttribute("data-session-id");
    expect(sessionId, "the active conversation has an id").toBeTruthy();

    await writeCheckpoint(
      app.gateway,
      checkpoint({
        runId: "run-panel-spec",
        goal: "add two numbers in the calculator",
        sessionId,
        steps: [
          { key: "super", result: "opened the launcher", verified: true, rung: "ax_action", effect: "confirmed" },
          { key: "3", result: "typed 3", verified: true, rung: "keyboard", effect: "confirmed" },
        ],
      }),
    );

    await page.getByTestId("titlebar-computer-panel").click();

    const panel = page.getByTestId("computer-panel");
    await expect(panel).toBeVisible();
    await expect(panel).toContainText("add two numbers in the calculator");
    // Both steps, in order, with the verdict each one earned.
    const steps = panel.getByTestId("computer-step");
    await expect(steps).toHaveCount(2);
    await expect(steps.nth(0)).toContainText("Step 1");
    await expect(steps.nth(0)).toContainText("opened the launcher");
    await expect(steps.nth(1)).toContainText("typed 3");

    // The cost line is the half of "人看得见" that a step list does not cover:
    // a run that spent a budget must say so.
    const cost = panel.getByTestId("computer-run-cost");
    await expect(cost).toBeVisible();
    await expect(cost).toContainText("2 steps");
    await expect(cost).toContainText("1500 tokens");
  });

  test("another conversation's run stays out of this one's panel", async ({ ui, page }) => {
    const app = await ui.launch();
    await app.send("first conversation");

    await writeCheckpoint(
      app.gateway,
      checkpoint({
        runId: "run-other-session",
        goal: "someone else's desktop work",
        sessionId: "a-session-this-page-never-opened",
        steps: [{ key: "a", result: "clicked", verified: true, rung: "pixel", effect: "confirmed" }],
      }),
    );

    await page.getByTestId("titlebar-computer-panel").click();
    const panel = page.getByTestId("computer-panel");
    await expect(panel).toBeVisible();

    // A shared gateway must not show one conversation another one's runs —
    // that is why the checkpoint records `session_id` at all.
    await expect(panel).not.toContainText("someone else's desktop work");
    await expect(panel).toContainText("Nothing is driving this desktop");
  });

  test("the panel shares the right pane with the workspace browser", async ({ ui, page }) => {
    const app = await ui.launch();
    await app.send("hello");

    await page.getByTestId("titlebar-computer-panel").click();
    await expect(page.getByTestId("computer-panel")).toBeVisible();

    // The two panes are mutually exclusive: one right pane, one occupant.
    await page.getByRole("button", { name: /browse workspace files/i }).click();
    await expect(page.getByTestId("computer-panel")).toHaveCount(0);
    await expect(page.getByRole("complementary", { name: /workspace files/i })).toBeVisible();
  });
});
