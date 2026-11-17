// The turn replay player: one turn's desktop actions, in order, with frames.
//
// The engine already answers `turns.replay` (see
// `tests/e2e/computer_run_tests.rs`); what this spec proves is the half that
// suite structurally cannot — that the timeline reaches the *screen*, that a
// step's frame is fetched by digest, and that the two halves of a turn (its
// own `computer` tool steps, and the steps of a run it started) render as one
// list in the order they happened.
//
// The data is written on disk, the same hermetic path `computer.spec.mjs`
// uses for checkpoints: a turn trace is what the gateway reads, and writing it
// directly needs neither a desktop adapter nor a model. The one thing that
// cannot be faked is the turn id the bubble carries — that comes from the
// real turn the fake provider answered, so the trace is written *under that
// id* after the answer lands.
import path from "node:path";
import { createHash } from "node:crypto";
import { mkdir, readdir, stat, writeFile } from "node:fs/promises";
import { expect, test } from "./fixtures/app.mjs";
import { plainText } from "./fixtures/scripts.mjs";

/** A 1×1 PNG — the smallest thing `sniff_mime` will still call an image. */
const PNG_BASE64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/** Store a frame in the attachment CAS the gateway reads, return its digest. */
async function storeFrame(gateway) {
  const bytes = Buffer.from(PNG_BASE64, "base64");
  const hex = createHash("sha256").update(bytes).digest("hex");
  const dir = path.join(gateway.home, "attachments", "sha256", hex.slice(0, 2));
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, hex.slice(2)), bytes);
  return `sha256:${hex}`;
}

/**
 * The directory the gateway itself wrote for this turn (`turns/<date>/<id>/`).
 *
 * Locating it rather than building the path matters: the gateway partitions by
 * *local* date, and a test that guessed a partition could write a second copy
 * next to the real one — `turns.replay` scans partitions and would then serve
 * whichever `read_dir` happened to list first.
 */
async function turnDir(gateway, turnId) {
  const base = path.join(gateway.home, "turns");
  const partitions = await readdir(base).catch(() => []);
  for (const name of partitions) {
    const candidate = path.join(base, name, turnId);
    if (await stat(candidate).then((s) => s.isDirectory()).catch(() => false)) {
      return candidate;
    }
  }
  throw new Error(`the gateway wrote no turn trace for ${turnId} under ${base}`);
}

/** Write a turn trace (`summary.json` + `full.json`) under the turn's own id. */
async function writeTurnTrace(gateway, turnId, { input, actions }) {
  const dir = await turnDir(gateway, turnId);
  await mkdir(dir, { recursive: true });
  await writeFile(
    path.join(dir, "summary.json"),
    JSON.stringify({ turn_id: turnId, user_message_preview: input }, null, 2),
  );
  // `full.json` is append-only JSONL; only `type: "action"` lines become
  // timeline steps, so a couple of other event kinds are here to prove they
  // are passed over rather than drawn.
  const lines = [
    { type: "round", round: 0, response: "ok" },
    ...actions.map((a) => ({
      type: "action",
      round: 0,
      action: a.action,
      detail: {
        action: a.action,
        rung: a.rung,
        effect: a.effect,
        delivery: "delivered",
        retry: "safe",
        verification: a.verification,
        before_ref: a.beforeRef ? { digest: a.beforeRef } : undefined,
        after_ref: a.afterRef ? { digest: a.afterRef } : undefined,
      },
    })),
  ];
  await writeFile(
    path.join(dir, "full.json"),
    lines.map((l) => JSON.stringify(l)).join("\n") + "\n",
  );
}

/** Write a run checkpoint this turn started (`computer-runs/<run_id>.json`). */
async function writeRunForTurn(gateway, { runId, goal, turnId, steps, afterRef }) {
  const dir = path.join(gateway.home, "computer-runs");
  await mkdir(dir, { recursive: true });
  await writeFile(
    path.join(dir, `${runId}.json`),
    JSON.stringify(
      {
        run_id: runId,
        goal,
        session_id: null,
        turn_id: turnId,
        max_steps: 30,
        settle_delay_ms: 200,
        verify_after_each: true,
        model_calls: steps.length,
        images_attached: 0,
        images_repeated: 0,
        tokens_in: 120,
        tokens_out: 30,
        updated_at: new Date().toISOString(),
        completed: steps.map((s, i) => ({
          step: i,
          action: { key_press: { keys: [s.key] } },
          verification: s.verified ? "verified" : "unverifiable",
          result: s.result,
          sensitive: false,
          rung: "ax_action",
          effect: "confirmed",
          screenshot_ref: afterRef,
        })),
      },
      null,
      2,
    ),
  );
}

/** The turn id the finished assistant bubble carries (stamped by `chat.final`). */
async function finishedTurnId(page) {
  const bubble = page.getByTestId("message-assistant").last();
  await expect(bubble).toHaveAttribute("data-turn-id", /.+/);
  return bubble.getAttribute("data-turn-id");
}

test.describe("turn replay player", () => {
  test("a turn's own steps and its run's steps play back as one timeline", async ({ ui }) => {
    const app = await ui.launch(plainText("I clicked and typed for you."));
    await app.send("open the calculator and add two numbers");
    const turnId = await finishedTurnId(app.page);

    // Two faces of the same screen: a step's before/after pair is what a
    // reader compares, and both are addresses in the CAS.
    const frame = await storeFrame(app.gateway);
    await writeTurnTrace(app.gateway, turnId, {
      input: "open the calculator and add two numbers",
      actions: [
        {
          action: "click",
          rung: "ax_action",
          effect: "confirmed",
          verification: "verified",
          beforeRef: frame,
          afterRef: frame,
        },
      ],
    });
    await writeRunForTurn(app.gateway, {
      runId: "run-replay-spec",
      goal: "add two numbers",
      turnId,
      afterRef: frame,
      steps: [
        { key: "3", result: "typed 3", verified: true },
        { key: "plus", result: "pressed plus", verified: true },
      ],
    });

    // The entry point lives on the finished answer, next to copy/regenerate.
    await app.page.getByTestId("message-assistant").last().getByRole("button", {
      name: /replay this turn's desktop actions/i,
    }).click();

    const player = app.page.getByTestId("turn-replay-player");
    await expect(player).toBeVisible();
    await expect(player).toContainText("open the calculator and add two numbers");

    // Three positions, in the order they happened, from two different files.
    const steps = player.getByTestId("turn-replay-step");
    await expect(steps).toHaveCount(3);
    await expect(steps.nth(0)).toHaveAttribute("data-source", "turn");
    await expect(steps.nth(1)).toHaveAttribute("data-source", "run");
    await expect(steps.nth(2)).toHaveAttribute("data-source", "run");
    await expect(steps.nth(1)).toContainText("typed 3");
    await expect(steps.nth(2)).toContainText("pressed plus");

    // The selected step's frame came back through `attachment.get` — the
    // digest in the trace is not something a browser can draw.
    await expect(player.getByTestId("turn-replay-frame")).toBeVisible();
    await expect(player.getByTestId("turn-replay-counter")).toContainText("1");

    // A step with both captures can be compared: the first one has one, so
    // both sides are offered and switching keeps a frame on screen.
    await expect(player.getByTestId("turn-replay-side-before")).toBeVisible();
    await player.getByTestId("turn-replay-side-before").click();
    await expect(player.getByTestId("turn-replay-side-before")).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect(player.getByTestId("turn-replay-frame")).toBeVisible();

    // Stepping forward changes the selection and the frame slot.
    await player.getByTestId("turn-replay-next").click();
    await expect(player.getByTestId("turn-replay-counter")).toContainText("2");
    await expect(steps.nth(1)).toHaveAttribute("data-active", "true");

    // The runs behind the timeline say what they cost — a timeline that shows
    // the steps but not the bill answers half the question.
    const cost = player.getByTestId("turn-replay-cost");
    await expect(cost).toBeVisible();
    await expect(cost).toContainText("2 steps");
    await expect(cost).toContainText("150 tokens");

    // Escape closes it; the message underneath is untouched.
    await app.page.keyboard.press("Escape");
    await expect(player).toHaveCount(0);
    await expect(app.lastAssistant).toContainText("I clicked and typed for you.");
  });

  test("a turn that never touched the desktop says so instead of drawing nothing", async ({ ui }) => {
    const app = await ui.launch(plainText("Just a sentence."));
    await app.send("say something");
    const turnId = await finishedTurnId(app.page);

    // A trace exists (the fake provider's turn was recorded) but carries no
    // action lines. That is not the same fact as "the gateway could not read
    // the timeline", and the player must not draw them the same way.
    await writeTurnTrace(app.gateway, turnId, { input: "say something", actions: [] });

    await app.page.getByTestId("message-assistant").last().getByRole("button", {
      name: /replay this turn's desktop actions/i,
    }).click();

    const player = app.page.getByTestId("turn-replay-player");
    await expect(player).toBeVisible();
    await expect(player).toContainText("This turn did not act on the desktop.");
    await expect(player.getByTestId("turn-replay-step")).toHaveCount(0);
  });
});
