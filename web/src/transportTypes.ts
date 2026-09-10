


export interface WsRequest {
  id: string;
  method: string;
  params?: Record<string, unknown>;
}

export interface WsEvent {
  event: string;
  payload?: Record<string, unknown>;
  seq?: number;
}

export type EventCallback = (evt: WsEvent) => void;
export type NetworkStatus = "connected" | "disconnected" | "connecting";
export type StatusCallback = (status: NetworkStatus) => void;

export interface ChatMessagePart {
  type: string;
  text?: string;
  toolName?: string;
  args?: Record<string, unknown>;
  result?: unknown;
  data?: unknown;
}

export interface ChatMessage {
  id: string;
  role: "user" | "assistant";
  content: string;
  parts?: ChatMessagePart[];
  timestamp?: number;
  /** Metadata: how many tools were called */
  toolCount?: number;
  /** Metadata: how long the response took (ms) */
  durationMs?: number;
  /** Live streaming status — set during response, cleared on complete */
  liveStatus?: {
    status: "thinking" | "tool_calling";
    toolName?: string;
  };
  /** Stable per-turn id from the `chat.final` event; the key for feedback.vote. */
  turnId?: string;
  /** Credits billed by the cloud relay for this turn (from `chat.final`
   * `usage.credits_used`). */
  credits?: number;
  /** Cloud credit balance after this turn (`usage.credit_balance`). */
  balanceAfter?: number;
  /** Machine-readable error code from `chat.error` (e.g.
   * `insufficient_credits`). */
  errorCode?: string;
}

export type MessagesCallback = (messages: ChatMessage[]) => void;
export type SessionCallback = () => void;

/** A concrete model owned by a provider, as returned by `models.list`. */
export interface ModelInfo {
  /** Selectable reference. Bare for local providers; `cloud/<bare>` for the
   * cloud proxy so a model id shared with a local provider stays
   * independently selectable. Persist this value when pinning. */
  id: string;
  /** Human-readable model name — always the bare id (no `cloud/` prefix). */
  name: string;
  provider: string;
  provider_name: string;
  /** Whether the provider has an API key configured (masked below). */
  has_api_key?: boolean;
  /** Masked API key for display (e.g. "sk-••••abcd"); never the raw value. */
  api_key_masked?: string;
  /** Provider base URL, when configured. */
  base_url?: string;
  /** Cloud billing multiplier (credits per 1K tokens); cloud models only. */
  credit_multiplier?: number;
}

/** First-launch identity form payload (WS `onboarding.apply`). */
export interface OnboardingPayload {
  /** Agent name / call sign. */
  name?: string;
  /** Short persona / vibe description. */
  vibe?: string;
  /** Signature emoji. */
  emoji?: string;
  /** How the user wants to be addressed. */
  user_name?: string;
  /** The user's city. */
  city?: string;
  /** Free-form context about the user. */
  user_context?: string;
}

/** Response from WS `onboarding.status`. */
export interface OnboardingStatus {
  status: "pending" | "done";
}

/* ── Eval dashboard (§八 评测看板) ── */

export interface EvalTraceSummary {
  id: string;
  kind: string;
  subject: string;
  status: string;
  decided_at: number;
}

export interface EvalOptimizerReport {
  run_id: string;
  started_at: number;
  finished_at: number;
  candidates_generated: number;
  applied: Array<{ path: string; from: number; to: number; new_revision: string }>;
  rejected: Array<{ path: string; reason: string }>;
  reason: string;
}

/** Aggregate returned by the read-only `eval.dashboard` method. */
export interface EvalDashboardPayload {
  traces: {
    total: number;
    by_kind: Record<string, number>;
    by_status: Record<string, number>;
    recent: EvalTraceSummary[];
  };
  badcases: {
    total: number;
    by_source: Record<string, number>;
    by_status: Record<string, number>;
  };
  feedback: {
    since_ms: number;
    up: number;
    down: number;
    total: number;
  };
  trends: {
    day: string;
    up: number;
    down: number;
    badcases: number;
    traces: number;
  }[];
  optimizer: {
    running: boolean;
    paused: boolean;
    breaker: { failures: number; tripped: boolean; open: boolean };
    last_run_at: number | null;
    last_report: EvalOptimizerReport | null;
    last_error: string | null;
  };
}

/** Aggregate returned by the read-only `feedback.ops` method. */
export interface FeedbackOpsPayload {
  since_ms: number;
  total_votes: number;
  up: number;
  down: number;
  by_agent: {
    agent_id: string;
    up: number;
    down: number;
    total: number;
  }[];
  pending_by_source: {
    source: string;
    count: number;
  }[];
  by_day: {
    day: string;
    up: number;
    down: number;
    total: number;
  }[];
  down_votes: {
    turn_id: string;
    input: string;
    risk_signals: string[];
  }[];
  risk_clusters: {
    label: string;
    count: number;
  }[];
}

/** A pending tool-call approval the human must decide. */
export interface ApprovalPrompt {
  approval_id: string;
  tool_name: string;
  requested_by: string;
  /** "Low" | "Medium" | "High" | "Critical" */
  risk_level: string;
  message: string;
}
