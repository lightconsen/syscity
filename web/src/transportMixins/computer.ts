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
