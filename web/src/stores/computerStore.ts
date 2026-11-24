import { create } from "zustand";
import type {
  ComputerRunPhase,
  ComputerRunRecord,
  ComputerRunStep,
  ComputerRunSummary,
  ComputerRunView,
} from "@/SyscityWebSocketTransport";

/** One step, as either the live event stream or the disk record describes it. */
export interface ComputerStepView {
  step: number;
  action: string;
  result?: string | null;
  verified?: boolean | null;
  rung?: string | null;
  effect?: string | null;
  beforeRef?: string | null;
  afterRef?: string | null;
  sensitive?: boolean;
  /** Where the step aimed, when its visible change landed elsewhere. */
  aimMissed?: { x: number; y: number; width: number; height: number } | null;
}

/** A run, merged from the live view and the file that outlives it. */
export interface ComputerRunEntry {
  runId: string;
  goal: string;
  phase: ComputerRunPhase;
  /** Steps the run has finished, zero-based ids. */
  steps: ComputerStepView[];
  summary?: ComputerRunSummary | null;
  message?: string | null;
  vision?: string | null;
  interrupted?: boolean;
  /** Seen from the live process (a status answer or a step event), so its
   *  phase is authoritative over anything derived from the file. */
  live?: boolean;
}

interface ComputerState {
  /** Keyed by run id: one entry either way, live or from disk. */
  runs: Record<string, ComputerRunEntry>;
  /** Run ids this panel has seen for the current conversation, newest last. */
  order: string[];
  selectedRunId: string | null;
  /** The conversation the entries belong to; runs are dropped on switch. */
  sessionId: string | null;

  selectRun: (runId: string | null) => void;
  /** Point the store at a conversation, discarding another one's runs. */
  bindSession: (sessionId: string | null) => void;
  mergeStatus: (view: ComputerRunView, sessionId?: string | null) => void;
  mergeRecord: (record: ComputerRunRecord) => void;
  /** Fold one `computer.step` event in. */
  applyStepEvent: (
    sessionId: string | null,
    runId: string,
    event: Record<string, unknown>,
    /** What the run was asked to do, as the gateway sends it with each event. */
    goal?: string | null
  ) => void;
}

/** The phase a checkpoint file implies, for a run this page never saw live.
 *
 * A file with no conclusion is an interrupted run — the plan's rule is that it
 * is resumable, not that it failed. This is a weaker statement than the live
 * view's, which is why [`ComputerRunEntry.live`] exists: once the process says
 * `running`, nothing derived from the file may overwrite it.
 */
function phaseFromRecord(record: ComputerRunRecord): ComputerRunPhase {
  switch (record.stop_reason) {
    case "done":
      return "done";
    case "cancelled":
      return "cancelled";
    case null:
    case undefined:
      return "paused";
    default:
      return "failed";
  }
}

function stepFromRecord(s: ComputerRunStep): ComputerStepView {
  return {
    step: s.step,
    action: s.action,
    result: s.result ?? null,
    verified: s.verified ?? null,
    rung: s.rung ?? null,
    effect: s.effect ?? null,
    beforeRef: s.before_ref ?? null,
    afterRef: s.after_ref ?? null,
    sensitive: s.sensitive ?? false,
    aimMissed: s.aim_missed ?? null,
  };
}

/** Insert or replace by step id, so a replayed event cannot duplicate a step. */
function upsertStep(steps: ComputerStepView[], next: ComputerStepView): ComputerStepView[] {
  const at = steps.findIndex((s) => s.step === next.step);
  if (at === -1) return [...steps, next].sort((a, b) => a.step - b.step);
  // The event's own facts win, but keep a result the disk record already had:
  // a step event says what ran, not what it printed.
  const merged = { ...steps[at], ...next, result: next.result ?? steps[at].result };
  return steps.map((s, i) => (i === at ? merged : s));
}

function withOrder(order: string[], runId: string): string[] {
  return order.includes(runId) ? order : [...order, runId];
}

/** Whether a phase means the run is still moving the machine. */
function isActive(phase: ComputerRunPhase): boolean {
  return phase === "running" || phase === "stepping";
}

/**
 * Which run the panel should be showing.
 *
 * `selected` is the person's own pick and outranks everything: choosing a run
 * to read must not be undone by a later event. Without one, a run that is
 * still moving the machine is what the panel is for — the previous behaviour
 * auto-selected the first run it ever saw and held it, so a panel opened after
 * one finished showed "Done" above a desktop a second run was driving. Only
 * when nothing is active does it fall back to the newest entry, which is what
 * a reader returning to a quiet conversation wants.
 */
export function pickShown(
  selected: string | null,
  order: string[],
  runs: Record<string, ComputerRunEntry>
): ComputerRunEntry | null {
  const explicit = selected ? runs[selected] : undefined;
  if (explicit) return explicit;
  const active = order
    .map((id) => runs[id])
    .filter((r): r is ComputerRunEntry => Boolean(r) && isActive(r.phase));
  if (active.length > 0) return active[active.length - 1];
  const newest = order.length > 0 ? runs[order[order.length - 1]] : undefined;
  return newest ?? null;
}

/**
 * A step result shortened for a one-line row.
 *
 * A `read the UI tree` step answers with the tree itself: hundreds of nodes of
 * JSON in a field sized for a sentence. Truncating that mid-token still shows
 * a wall of punctuation, so a JSON result is summarised as what it holds — the
 * node count and the applications it walked — and the full text stays behind
 * the row's disclosure. Presentation only; the model reads the whole result.
 */
export const RESULT_PREVIEW_LIMIT = 160;

/** Node count and application names of a UI-tree result, when that is what it is. */
function describeTree(result: string): string | null {
  if (!result.startsWith("[") && !result.startsWith("{")) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(result);
  } catch {
    // Not JSON — but it starts like it. Say nothing rather than guess.
    return null;
  }
  const roots = Array.isArray(parsed) ? parsed : null;
  if (!roots) return null;
  let nodes = 0;
  const apps: string[] = [];
  const walk = (n: unknown, depth: number) => {
    if (!n || typeof n !== "object") return;
    nodes += 1;
    const node = n as { role?: unknown; label?: unknown; value?: unknown; children?: unknown };
    if (depth <= 1) {
      const role = typeof node.role === "string" ? node.role : "";
      const name =
        (typeof node.label === "string" && node.label) ||
        (typeof node.value === "string" && node.value) ||
        "";
      if (role === "application" && name) apps.push(name);
    }
    if (Array.isArray(node.children)) for (const c of node.children) walk(c, depth + 1);
  };
  for (const r of roots) walk(r, 0);
  const names = apps.length ? `: ${apps.slice(0, 4).join(", ")}${apps.length > 4 ? "…" : ""}` : "";
  return `${nodes} elements${names}`;
}

export function previewResult(result: string, limit = RESULT_PREVIEW_LIMIT): string {
  const tree = describeTree(result);
  if (tree) return tree;
  const flat = result.replace(/\s+/g, " ").trim();
  return flat.length > limit ? `${flat.slice(0, limit)}…` : flat;
}

export function isLongResult(result: string | null | undefined, limit = RESULT_PREVIEW_LIMIT): boolean {
  return Boolean(result && result.length > limit);
}

export const useComputerStore = create<ComputerState>((set) => ({
  runs: {},
  order: [],
  selectedRunId: null,
  sessionId: null,

  selectRun: (selectedRunId) => set({ selectedRunId }),

  // A run belongs to the conversation that asked for it. Using one store for
  // every session would let yesterday's run show up under today's goal, which
  // is exactly the confusion the panel exists to end.
  bindSession: (sessionId) =>
    set((s) =>
      s.sessionId === sessionId
        ? s
        : { sessionId, runs: {}, order: [], selectedRunId: null }
    ),

  mergeStatus: (view, sessionId) =>
    set((s) => {
      const prev = s.runs[view.run_id];
      const fromStatus: ComputerStepView[] = view.completed_steps.map(
        (line: string, i: number) => ({ step: i, action: line })
      );
      // Event steps carry frame refs the status list does not, so they win when
      // both exist; the status list is the only source after a reload.
      const steps = prev?.steps.length
        ? prev.steps
        : fromStatus;
      return {
        runs: {
          ...s.runs,
          [view.run_id]: {
            ...prev,
            runId: view.run_id,
            goal: view.goal,
            phase: view.phase,
            steps,
            summary: view.summary ?? null,
            message: view.message ?? null,
            vision: view.vision ?? null,
            // `paused` is the phase a run with no conclusion has, so it is also
            // the only phase that is interrupted. A status answer for a run
            // that has since reached `done` must not leave a chip behind that
            // says it was cut off — the panel would show "Done" and
            // "Interrupted" side by side.
            interrupted: view.phase === "paused",
            live: true,
          },
        },
        order: withOrder(s.order, view.run_id),
        // No selection is made here. Which run to show is a question asked of
        // the whole set (an active one outranks a finished one), and answering
        // it by auto-selecting the first run seen is how the panel came to be
        // pinned to a finished run while another was driving the desktop.
        sessionId: sessionId ?? s.sessionId,
      };
    }),

  mergeRecord: (record) =>
    set((s) => {
      const prev = s.runs[record.run_id];
      // The disk record is the only source of each step's result text and frame
      // refs once the process is gone; the live stream wins on freshness.
      const recordSteps = record.steps.map(stepFromRecord);
      const steps =
        prev && prev.steps.length >= recordSteps.length
          ? recordSteps.reduce(upsertStep, prev.steps)
          : recordSteps;
      // The disk cannot tell "paused by a person" from "the process died
      // mid-step" — both are a file with no stop reason. A live entry does
      // know, so it wins; the derived phase is only for a run this page never
      // saw.
      const phase: ComputerRunPhase = prev?.live ? prev.phase : phaseFromRecord(record);
      return {
        runs: {
          ...s.runs,
          [record.run_id]: {
            ...prev,
            runId: record.run_id,
            goal: record.goal,
            phase,
            steps,
            // The same rule as the status merge: only a run with no conclusion
            // is interrupted. Re-deriving it from the phase keeps a stale
            // "interrupted" from a record read mid-run out of a finished tab.
            interrupted: phase === "paused",
            // The cost of a run is the run's, not its conclusion's: an
            // interrupted run spent the same budget as one that stopped for a
            // reason, and showing no cost line for it would understate what a
            // person actually paid. `stop_reason` is null when the file has
            // none — the phase, not the summary, is what says why it ended.
            summary:
              prev?.summary ??
              {
                steps: record.steps_completed,
                model_calls: record.model_calls,
                images_attached: record.images_attached,
                images_repeated: record.images_repeated ?? 0,
                tokens_in: record.tokens_in,
                tokens_out: record.tokens_out,
                stop_reason: record.stop_reason ?? null,
              },
          },
        },
        order: withOrder(s.order, record.run_id),
      };
    }),

  applyStepEvent: (sessionId, runId, event, goal) =>
    set((s) => {
      // The live stream broadcasts to every client on the gateway. A run that
      // belongs to another conversation must stay out of this panel for the
      // same reason `computer.runs` is filtered by `session_id`: one person's
      // desktop work is not another's to watch.
      if (sessionId && s.sessionId && sessionId !== s.sessionId) return s;
      const kind = event.kind;
      const prev = s.runs[runId];
      if (kind === "finished") {
        const phase = (event.phase as ComputerRunPhase) ?? "failed";
        const message = (event.message as string) ?? "";
        const existing = prev ?? {
          runId,
          goal: goal ?? "",
          phase,
          steps: [],
        };
        return {
          runs: {
            ...s.runs,
            [runId]: {
              ...existing,
              phase,
              message,
              live: true,
              // The event names the run; a reader that never saw an opening
              // status gets its goal from whichever event arrives first.
              goal: existing.goal || goal || "",
              // A run that just announced its conclusion is not interrupted,
              // whatever a disk read taken while it was still stepping said.
              // The phase, not the event name, decides: one rule for all three
              // merge paths.
              interrupted: phase === "paused",
              summary: existing.summary
                ? { ...existing.summary, stop_reason: String(event.stop_reason ?? "done") }
                : null,
            },
          },
          // A run that started outside this conversation never had a session,
          // so binding one here would mislabel it as belonging to whatever was
          // open.
          sessionId: sessionId ?? s.sessionId,
        };
      }
      if (kind !== "step") return s;
      const step: ComputerStepView = {
        step: (event.step as number) ?? 0,
        action: (event.action as string) ?? "",
        verified: (event.verified as boolean | null) ?? null,
        rung: (event.rung as string | null) ?? null,
        effect: (event.effect as string | null) ?? null,
        beforeRef: (event.screenshot_before_ref as string | null) ?? null,
        afterRef: (event.screenshot_ref as string | null) ?? null,
        // Present only when the step's visible change landed away from where
        // it aimed; `null` covers both "verified" and "nothing checked it".
        aimMissed:
          (event.aim_missed as ComputerStepView["aimMissed"]) ?? null,
      };
      const existing = prev ?? {
        runId,
        goal: goal ?? "",
        phase: "running" as ComputerRunPhase,
        steps: [],
      };
      return {
        runs: {
          ...s.runs,
          [runId]: {
            ...existing,
            goal: existing.goal || goal || "",
            steps: upsertStep(existing.steps, step),
            live: true,
          },
        },
        order: withOrder(s.order, runId),
        sessionId: sessionId ?? s.sessionId,
      };
    }),
}));
