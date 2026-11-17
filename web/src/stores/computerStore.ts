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
    event: Record<string, unknown>
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
            live: true,
          },
        },
        order: withOrder(s.order, view.run_id),
        // Keep an explicit selection; otherwise follow the newest run, which is
        // what a person who just asked for one expects to be looking at.
        selectedRunId: s.selectedRunId ?? view.run_id,
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
            interrupted: record.interrupted,
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
        selectedRunId: s.selectedRunId ?? record.run_id,
      };
    }),

  applyStepEvent: (sessionId, runId, event) =>
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
          goal: "",
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
      };
      const existing = prev ?? {
        runId,
        goal: "",
        phase: "running" as ComputerRunPhase,
        steps: [],
      };
      return {
        runs: {
          ...s.runs,
          [runId]: {
            ...existing,
            steps: upsertStep(existing.steps, step),
            live: true,
          },
        },
        order: withOrder(s.order, runId),
        selectedRunId: s.selectedRunId ?? runId,
        sessionId: sessionId ?? s.sessionId,
      };
    }),
}));
