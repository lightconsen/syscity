import { test, expect } from "./fixtures/app.mjs";
import { readOnlyTool } from "./fixtures/scripts.mjs";

/**
 * A turn that calls a tool, seen from the screen.
 *
 * The Rust e2e suite proves the tool ran. These tests prove the transcript
 * shows it: the collapsible internals line appears, the panel auto-opens for a
 * fresh turn (so a running tool is visible without a click), and closing it
 * hides the call again.
 *
 * A read-only `glob` is used so the call is not gated: this spec is about
 * rendering, and `approval.spec.mjs` covers the gate.
 */
test.describe("tool calls in the transcript", () => {
  test("a tool call is shown, and can be collapsed", async ({ ui }) => {
    const app = await ui.launch(readOnlyTool());

    await app.send("list the markdown files");

    const bubble = app.lastAssistant;
    const toggle = bubble.getByTestId("internals-toggle");
    await expect(toggle).toBeVisible();
    // A new turn with internals opens them by default — the person watching
    // the answer should not have to find a disclosure triangle to see what
    // the agent is doing.
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    // The summary names the count, not the ids, so a second call changes the
    // line rather than adding a row.
    await expect(toggle).toContainText("1");

    const panel = bubble.getByTestId("internals-panel");
    await expect(panel).toHaveAttribute("data-expanded", "true");

    // The card carries the tool's own name and its final status — the tool
    // name is what a user uses to recognise the call.
    const card = bubble.getByTestId("tool-call");
    await expect(card).toBeVisible();
    await expect(card).toHaveAttribute("data-tool-name", "glob");
    await expect(card).toHaveAttribute("data-status", "done");

    // The card is expandable: its arguments are hidden until asked for.
    await expect(card).not.toContainText('"pattern"');
    await card.getByRole("button").first().click();
    await expect(card).toContainText('"pattern": "*.md"');

    // The answer that follows the tool call is ordinary text.
    await expect(bubble).toContainText("found the files");

    // Collapsing hides the call but keeps the summary line.
    await toggle.click();
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await expect(panel).toHaveAttribute("data-expanded", "false");
    await expect(bubble.getByTestId("tool-call")).toBeHidden();
    await expect(bubble).toContainText("found the files");
  });
});
