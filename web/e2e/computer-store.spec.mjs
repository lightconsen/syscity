// The desktop-run panel's merge rules, without a browser or a desktop.
//
// Every defect this store has shipped was a *merge* defect: a finished run
// labelled interrupted, a live run's goal missing because the step event was
// the first thing that named it, the panel pinned to whichever run it happened
// to see first. Each was invisible in the gateway's own tests because the
// gateway was right — the client folded correct facts into a wrong picture.
// These tests drive the three fold paths directly.
import { test, expect } from "@playwright/test";
import {
  pickShown,
  previewResult,
  useComputerStore,
} from "../src/stores/computerStore.ts";

/** A run as the store holds it, for the selection rules. */
function entry(runId, phase) {
  return { runId, goal: `goal of ${runId}`, phase, steps: [] };
}

test.beforeEach(() => {
  useComputerStore.getState().bindSession("s1");
});

test.describe("run selection", () => {
  test("a run that is still moving the desktop outranks a finished one", () => {
    const runs = {
      old: entry("old", "done"),
      live: entry("live", "running"),
    };
    // The old behaviour auto-selected the first run ever seen and held it, so
    // the panel showed "Done" above a desktop a second run was driving.
    expect(pickShown(null, ["old", "live"], runs)?.runId).toBe("live");
  });

  test("the newest active run wins when there are several", () => {
    const runs = {
      a: entry("a", "running"),
      b: entry("b", "stepping"),
      done: entry("done", "done"),
    };
    expect(pickShown(null, ["a", "done", "b"], runs)?.runId).toBe("b");
  });

  test("with nothing active the newest entry is shown", () => {
    const runs = { a: entry("a", "done"), b: entry("b", "failed") };
    expect(pickShown(null, ["a", "b"], runs)?.runId).toBe("b");
  });

  test("a person's own pick outranks an active run", () => {
    const runs = { picked: entry("picked", "done"), live: entry("live", "running") };
    expect(pickShown("picked", ["picked", "live"], runs)?.runId).toBe("picked");
  });

  test("an empty store shows nothing rather than an empty run", () => {
    expect(pickShown(null, [], {})).toBeNull();
  });
});

test.describe("live step events", () => {
  test("the first event names the run, even though it is only a step", () => {
    useComputerStore
      .getState()
      .applyStepEvent(
        "s1",
        "run-1",
        { kind: "step", step: 0, action: "click [s1:0.4]", verified: true },
        "open a terminal and run uname -a"
      );
    const run = useComputerStore.getState().runs["run-1"];
    expect(run.goal).toBe("open a terminal and run uname -a");
    expect(run.steps).toHaveLength(1);
    expect(run.phase).toBe("running");
  });

  test("a run that announces its end is not interrupted", () => {
    const store = useComputerStore.getState();
    store.mergeRecord({
      run_id: "run-2",
      goal: "do a thing",
      session_id: "s1",
      steps_completed: 0,
      model_calls: 0,
      images_attached: 0,
      tokens_in: 0,
      tokens_out: 0,
      stop_reason: null,
      steps: [],
    });
    // Read mid-run: the file has no conclusion, so it reads as interrupted.
    expect(useComputerStore.getState().runs["run-2"].interrupted).toBe(true);

    // The live process then says it finished; the stale flag must go, or the
    // panel draws "Done" and "Interrupted" side by side.
    useComputerStore
      .getState()
      .applyStepEvent(
        "s1",
        "run-2",
        { kind: "finished", phase: "done", message: "all done", stop_reason: "done" },
        "do a thing"
      );
    const run = useComputerStore.getState().runs["run-2"];
    expect(run.phase).toBe("done");
    expect(run.interrupted).toBe(false);
  });

  test("another conversation's events stay out", () => {
    useComputerStore
      .getState()
      .applyStepEvent("someone-else", "run-3", { kind: "step", step: 0, action: "click" });
    expect(useComputerStore.getState().runs["run-3"]).toBeUndefined();
  });
});

test.describe("step result previews", () => {
  test("a UI tree is summarised as what it holds", () => {
    const tree = JSON.stringify([
      {
        role: "application",
        label: "Terminal",
        children: [{ role: "frame", label: "x", children: [] }],
      },
    ]);
    expect(previewResult(tree)).toBe("2 elements: Terminal");
  });

  test("a plain result is shortened to one line", () => {
    const long = "word ".repeat(100);
    const preview = previewResult(long);
    expect(preview.length).toBeLessThan(long.length);
    expect(preview.endsWith("…")).toBe(true);
    // Newlines in a short result are flattened, since the row is one line.
    expect(previewResult("a\nb")).toBe("a b");
  });
});
