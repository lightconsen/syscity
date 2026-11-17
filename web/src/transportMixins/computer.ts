import type { SyscityWebSocketTransport } from "../transportCore";

// Domain mixin: desktop-automation (computer use) RPC method implementations
// (installed on the prototype by the facade `SyscityWebSocketTransport.ts`).
// Signatures are merged onto the class type in `transportCore.ts`.
//
// Read-only on purpose: the panel these feed can look at a run but cannot
// pause, cancel or start one. `computer.control` and `computer.run` would each
// be one more way to make a machine do something, and the panel exists because
// a person could not *see* what was already happening.
export function install(proto: typeof SyscityWebSocketTransport.prototype): void {
  /** One run's live state, or every live/finished run when `runId` is omitted. */
  proto.computerStatus = async function (
    this: SyscityWebSocketTransport,
    runId?: string
  ): Promise<ComputerStatusAnswer | null> {
    try {
      const params: Record<string, unknown> = {};
      if (runId) params.run_id = runId;
      return (await this.sendRequestAndWait("computer.status", params, 10_000)) as
        | ComputerStatusAnswer
        | null;
    } catch {
      return null;
    }
  };

  /** Runs recorded on disk — the only view that survives a gateway restart. */
  proto.computerRuns = async function (
    this: SyscityWebSocketTransport,
    includeFinished = true
  ): Promise<{ runs: ComputerRunRecord[]; interrupted: number; finished: number }> {
    try {
      const res = (await this.sendRequestAndWait(
        "computer.runs",
        { include_finished: includeFinished },
        10_000
      )) as { runs?: ComputerRunRecord[]; interrupted?: number; finished?: number } | undefined;
      return {
        runs: res?.runs ?? [],
        interrupted: res?.interrupted ?? 0,
        finished: res?.finished ?? 0,
      };
    } catch {
      return { runs: [], interrupted: 0, finished: 0 };
    }
  };

  /**
   * One finished (or running) turn as an ordered action/frame timeline.
   *
   * Answers both halves of a turn's desktop work: the `computer` tool's steps
   * from the turn's own trace, and the steps of any run the turn started, in
   * the order they happened. `source` on each step says which half it came
   * from, so a reader does not have to guess.
   *
   * `null` when the gateway cannot answer (not found, or the socket dropped):
   * a timeline that is not there is not an empty timeline, and the player
   * says so instead of drawing "this turn did nothing".
   */
  proto.turnsReplay = async function (
    this: SyscityWebSocketTransport,
    turnId: string
  ): Promise<TurnReplay | null> {
    try {
      return (await this.sendRequestAndWait(
        "turns.replay",
        { turn_id: turnId },
        15_000
      )) as TurnReplay | null;
    } catch {
      return null;
    }
  };

  /**
   * One stored frame, by the digest a step carries.
   *
   * Returns a `data:` URL rather than the base64 body so the caller can hand
   * it straight to an `<img src>`: the mime type is sniffed by the gateway from
   * the bytes, and guessing it from the digest (or from the file extension of
   * something no longer on disk) is how a good frame renders as a broken image.
   */
  proto.computerFrameUrl = async function (
    this: SyscityWebSocketTransport,
    digest: string
  ): Promise<string | null> {
    try {
      const res = (await this.sendRequestAndWait("attachment.get", { digest }, 15_000)) as
        | { mime?: string; content_base64?: string }
        | undefined;
      if (!res?.content_base64) return null;
      return `data:${res.mime || "image/png"};base64,${res.content_base64}`;
    } catch {
      // A frame that is gone (swept, or captured on another machine) is not an
      // error the panel should shout about — the step is still worth reading.
      return null;
    }
  };
}

/** The cost line a run ends with.
 *
 * `stop_reason` is null for a run the file has no conclusion for — an
 * interrupted one. It still spent its budget, so the cost is still its own.
 */
export interface ComputerRunSummary {
  steps: number;
  model_calls: number;
  images_attached: number;
  images_repeated?: number;
  tokens_in: number;
  tokens_out: number;
  stop_reason: string | null;
}

export type ComputerRunPhase = "running" | "paused" | "stepping" | "cancelled" | "done" | "failed";

/** The live view (`computer.status`), which only exists while the process does. */
export interface ComputerRunView {
  run_id: string;
  goal: string;
  phase: ComputerRunPhase;
  runtime_state?: string;
  step: number;
  last_action?: string | null;
  last_verified?: boolean | null;
  consecutive_failures?: number;
  last_error?: string | null;
  /** One human sentence per completed step, oldest first. */
  completed_steps: string[];
  model_calls: number;
  images_attached: number;
  summary?: ComputerRunSummary | null;
  /** The last step's visual-scan verdict, or why nobody could look. */
  vision?: string | null;
  message?: string | null;
  /** The conversation that started the run, when one did. */
  session_id?: string | null;
  finished: boolean;
}

/** What `computer.status` answers: a single run, or a list plus watchers. */
export interface ComputerStatusAnswer extends Partial<ComputerRunView> {
  runs?: ComputerRunView[];
  watches?: Array<{ watch_id: string; interval_ms: number; stopping: boolean }>;
}

/** One step as the on-disk record keeps it. */
export interface ComputerRunStep {
  step: number;
  action: string;
  result?: string | null;
  verified?: boolean | null;
  before_ref?: string | null;
  after_ref?: string | null;
  sensitive?: boolean;
  rung?: string | null;
  effect?: string | null;
  /**
   * Where the step aimed, when its visible change landed somewhere else.
   *
   * The actionable half of a `null` verdict: `null` alone cannot tell "nothing
   * checked this" from "the screen moved, but not at the target".
   */
  aim_missed?: { x: number; y: number; width: number; height: number } | null;
}

/** The disk view (`computer.runs`) — the run's record after the process. */
export interface ComputerRunRecord {
  run_id: string;
  goal: string;
  session_id?: string | null;
  turn_id?: string | null;
  steps_completed: number;
  next_step: number;
  model_calls: number;
  images_attached: number;
  images_repeated?: number;
  tokens_in: number;
  tokens_out: number;
  interrupted: boolean;
  stop_reason?: string | null;
  sensitive_steps?: number;
  updated_at: string;
  completed_steps: string[];
  steps: ComputerRunStep[];
}

/**
 * One position on a turn's timeline, whichever half of the turn wrote it.
 *
 * A turn can act on the desktop two ways: the `computer` tool inside the turn
 * (its steps live in the turn trace) or a `computer` run the turn started (its
 * steps live in the run's checkpoint). `turns.replay` answers both in this one
 * shape, so the player draws one list; `source` is the only thing that says
 * which file a row came from.
 *
 * Fields a source cannot fill are `null`, not absent: an omitted key would
 * read as "this gateway version does not have it", which is a different fact
 * from "a run's steps have no round number".
 */
export interface TurnReplayStep {
  /** Zero-based position across the whole timeline, in the order it happened. */
  step: number;
  /** Which half of the turn produced it. */
  source: "turn" | "run";
  /** The action, described semantically (`click element s1:0.2`, `press cmd+s`). */
  action: string | null;
  /** Verification verdict: `true` verified, `false` measured failure, `null`
   *  nothing could say either way. */
  verified: boolean | null;
  rung?: string | null;
  effect?: string | null;
  /** Why the step stopped where it did (`turn` steps only). */
  delivery?: string | null;
  recovery?: string | null;
  settle_ms?: number | null;
  round?: number | null;
  /** CAS digest of the frame captured before the action landed. */
  before_ref?: string | null;
  /** CAS digest of the frame captured after the action settled. */
  after_ref?: string | null;
  /** The run step's own words for what happened (`run` steps only). */
  result?: string | null;
  /** Whether the step touched a sensitive control (`run` steps only). */
  sensitive?: boolean;
  /**
   * Where a step aimed, when the screen changed somewhere else.
   *
   * `verified: null` covers two different facts: nothing could check the
   * step, and the step visibly missed. This box is present only for the
   * second, so a timeline can say "the change was elsewhere" instead of
   * leaving a person to guess.
   */
  aim_missed?: { x: number; y: number; width: number; height: number } | null;
  /** The run's own step number, so a row can be found in `computer.status`. */
  run_step?: number;
  /** Which run contributed the step (`run` steps only). */
  run_id?: string;
}

/** The cost line of one run folded into a turn's timeline. */
export interface TurnReplayRun {
  run_id: string;
  goal: string;
  steps: number;
  model_calls: number;
  images_attached: number;
  images_repeated: number;
  tokens_in: number;
  tokens_out: number;
  stop_reason: string | null;
}

/**
 * `turns.replay`'s answer: one turn as the ordered list of what it did.
 *
 * `image_method` names the read that turns a digest into pixels — it is
 * `"attachment.get"` today, and it travels in the payload so a UI does not
 * hardcode a scheme that another version could replace.
 */
export interface TurnReplay {
  turn_id: string;
  /** The turn's input, so the timeline says what the steps answered. */
  input: string;
  steps: TurnReplayStep[];
  step_count: number;
  /** Frames shown on the timeline (not the same as `steps.length`). */
  image_count: number;
  image_method: string;
  /** The turn's summary record, when a trace was written for it. */
  summary: Record<string, unknown> | null;
  runs: TurnReplayRun[];
}
