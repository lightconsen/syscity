// Core transport: connection/session/chat machinery + ChatModelAdapter.
// Domain RPC methods are installed by transportMixins/* via the facade
// (`SyscityWebSocketTransport.ts`), which also owns the active-transport
// singleton.

import type {
  ChatModelAdapter,
  ChatModelRunOptions,
  ChatModelRunResult,
  TextMessagePart,
  ReasoningMessagePart,
  ToolCallMessagePart,
} from "@assistant-ui/react";

import {
  parseCommand,
  findCommand,
  LOCAL_COMMANDS,
} from "./slash-commands";

import { getGatewayBase, setGatewayBase } from "./lib/gatewayBase";

import type {
  WsEvent,
  EventCallback,
  NetworkStatus,
  StatusCallback,
  ChatMessagePart,
  ChatMessage,
  MessagesCallback,
  SessionCallback,
  ModelInfo,
  OnboardingPayload,
  OnboardingStatus,
  EvalDashboardPayload,
  FeedbackOpsPayload,
} from "./transportTypes";

export type * from "./transportTypes";
export * from "./transportTypes";

export class SyscityWebSocketTransport implements ChatModelAdapter {
  private ws: WebSocket | null = null;
  private reqId = 0;
  private sessionId: string;
  /** Per-session event queues, keyed by session_id. '' key for non-session events. */
  private eventQueues: Map<string, WsEvent[]> = new Map();
  /** Per-session event waiters. */
  private eventWaiters: Map<string, Array<(evt: WsEvent | null) => void>> = new Map();
  /** Per-session message arrays. */
  private messagesMap: Map<string, ChatMessage[]> = new Map();
  /** Set of session_ids currently being generated. */
  private runningSessions: Set<string> = new Set();
  /** Per-session AbortControllers for local cancellation. */
  private abortControllers: Map<string, AbortController> = new Map();
  /** Sessions whose background processing has completed. */
  private runningSessionListeners: Set<(ids: string[]) => void> = new Set();
  private reconnectDelay = 800;
  private readonly reconnectCap = 15000;
  private readonly reconnectMultiplier = 1.7;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  /** Liveness watchdog: a frozen gateway leaves pings unanswered and keeps the
   *  TCP connection open, so `onclose` never fires. A heartbeat probe + timeout
   *  force-closes so the reconnect path self-heals. */
  private heartbeatTimer: ReturnType<typeof setInterval> | null = null;
  private connectTimeoutTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly heartbeatIntervalMs = 15000;
  private readonly heartbeatTimeoutMs = 10000;
  private readonly connectTimeoutMs = 20000;
  private deviceId: string;
  private subscribedSessions: string[] = [];
  private listeners: Set<EventCallback> = new Set();
  private statusListeners: Set<StatusCallback> = new Set();
  private responseWaiters = new Map<
    string,
    { resolve: (v: unknown) => void; reject: (e: Error) => void }
  >();
  private currentStatus: NetworkStatus = "connecting";
  private messages: ChatMessage[] = [];
  /** New Session welcome page armed: the next send creates the real session
   *  first (deferred creation — no session exists until the first message). */
  private pendingNewSession = false;
  /** Agent to bind to the deferred session (armed via armNewSession(agentId),
   *  consumed by the first send). */
  private pendingAgentId: string | undefined = undefined;
  private messagesListeners: Set<MessagesCallback> = new Set();
  private sessionListeners: Set<SessionCallback> = new Set();
  private runListeners: Set<(running: boolean) => void> = new Set();
  private serverInfo: { version?: string; conn_id?: string; features?: string[]; scopes_granted?: string[] } = {};
  private gatewayUrl: string = "";
  /** Per-install gateway auth token (mobile Tauri builds only). */
  private gatewayToken: string | null = null;

  constructor() {
    this.deviceId =
      localStorage.getItem("syscity_device_id") || this.generateId();
    localStorage.setItem("syscity_device_id", this.deviceId);

    const storedSession = localStorage.getItem("syscity_session");
    this.sessionId = storedSession || `web:${this.deviceId}`;
    localStorage.setItem("syscity_session", this.sessionId);

    const isTauri = typeof window !== "undefined" && "__TAURI__" in window;
    if (isTauri) {
      // In Tauri, wait for the gateway-ready event so we know the backend
      // is fully initialized and get the actual port (port auto-detection).
      this.waitForTauriGateway();
    } else {
      this.connect();
    }
  }

  /** In Tauri mode, listen for the `gateway-ready` event from the backend. */
  private async waitForTauriGateway() {
    try {
      const { listen } = await import("@tauri-apps/api/event");
      const { invoke } = await import("@tauri-apps/api/core");
      // Pre-resolve the gateway URL from the Tauri command (port already detected).
      try {
        const apiUrl = await invoke<string>("get_api_url");
        setGatewayBase(apiUrl);
        this.gatewayUrl = apiUrl.replace(/^http/, "ws") + "/ws";
      } catch {
        // Fallback if command unavailable
      }
      // Mobile builds require the per-install gateway token (loopback is
      // shared with other apps). Desktop builds don't register the command.
      try {
        const token = await invoke<string | null>("get_gateway_token");
        if (token) {
          this.gatewayToken = token;
          // Shared with plain-HTTP fetches (e.g. artifact preview).
          localStorage.setItem("syscity_gateway_token", token);
        }
      } catch {
        // Not a mobile build — no token required.
      }
      // Wait for the gateway-ready event so we know the backend is listening.
      await listen<string>("gateway-ready", (event) => {
        const apiUrl = event.payload;
        setGatewayBase(apiUrl);
        this.gatewayUrl = apiUrl.replace(/^http/, "ws") + "/ws";
        this.connect();
      });
    } catch {
      // Fallback: connect anyway with best guess URL
      this.gatewayUrl = "ws://127.0.0.1:18080/ws";
      this.connect();
    }
  }

  onEvent(callback: EventCallback): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }

  onStatusChange(callback: StatusCallback): () => void {
    this.statusListeners.add(callback);
    callback(this.currentStatus);
    return () => this.statusListeners.delete(callback);
  }

  onSessionChange(callback: SessionCallback): () => void {
    this.sessionListeners.add(callback);
    return () => this.sessionListeners.delete(callback);
  }

  onRunStateChange(callback: (running: boolean) => void): () => void {
    this.runListeners.add(callback);
    callback(this.runningSessions.has(this.sessionId));
    return () => this.runListeners.delete(callback);
  }

  onRunningSessionsChange(callback: (ids: string[]) => void): () => void {
    this.runningSessionListeners.add(callback);
    callback(Array.from(this.runningSessions));
    return () => this.runningSessionListeners.delete(callback);
  }

  /** Full abort: local AbortController + sends chat.abort to backend. Used by Stop button. */
  abort(sessionId?: string): void {
    const sid = sessionId ?? this.sessionId;
    this.abortLocal(sid);
    this.sendRequest("chat.abort", {
      session_id: sid,
    });
    this.runningSessions.delete(sid);
    this.notifyRunningSessionsChanged();
  }

  /** Local-only abort: cancels frontend generator without backend chat.abort. Used by session switch. */
  abortLocal(sessionId?: string): void {
    const sid = sessionId ?? this.sessionId;
    const controller = this.abortControllers.get(sid);
    if (controller) {
      controller.abort();
      this.abortControllers.delete(sid);
    }
    this.runningSessions.delete(sid);
    this.notifyRunningSessionsChanged();
  }

  private notifyRunningSessionsChanged(): void {
    const ids = Array.from(this.runningSessions);
    this.runningSessionListeners.forEach((cb) => cb(ids));
    // Keep backwards compatible: notify run listeners with current session state
    const currentRunning = this.runningSessions.has(this.sessionId);
    this.runListeners.forEach((cb) => cb(currentRunning));
  }

  private notifySessionChange() {
    this.sessionListeners.forEach((cb) => cb());
  }

  getSessionId(): string {
    return this.sessionId;
  }

  private setStatus(status: NetworkStatus) {
    if (this.currentStatus === status) return;
    this.currentStatus = status;
    this.statusListeners.forEach((cb) => cb(status));
  }

  private generateId(): string {
    return "dev_" + Math.random().toString(36).slice(2, 10);
  }

  private connect() {
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    // Fresh watchdog state for this attempt (old timers must not outlive the
    // previous connection or close a newer one).
    this.stopHeartbeat();
    this.clearConnectTimeout();

    this.setStatus("connecting");

    // In Tauri WebView the Gateway runs on localhost, not the WebView's own origin.
    const isTauri = typeof window !== "undefined" && "__TAURI__" in window;

    let url: string;
    if (isTauri && this.gatewayUrl) {
      url = this.gatewayUrl;
    } else if (isTauri) {
      url = "ws://127.0.0.1:18080/ws";
    } else {
      // Browser: the gateway base is same-origin unless a remote base is
      // stored (web Connection settings); derive the ws URL from it.
      url = getGatewayBase().replace(/^http/, "ws") + "/ws";
    }

    // Mobile gateways require the shared token at the WS *upgrade* (before
    // any connect message arrives); browsers can't set headers on WebSocket,
    // so the token goes in the query string (`?token=` is accepted by
    // `gateway/ws/core.rs`).
    if (this.gatewayToken) {
      const sep = url.includes("?") ? "&" : "?";
      url = `${url}${sep}token=${encodeURIComponent(this.gatewayToken)}`;
    }

    this.gatewayUrl = url;
    this.ws = new WebSocket(url);
    const ws = this.ws;
    // A gateway that never completes the connect handshake is also wedged
    // (accepts the TCP upgrade but stops responding). Force-close if we
    // haven't reached "connected" in time so the reconnect path self-heals.
    this.connectTimeoutTimer = setTimeout(() => {
      if (this.ws === ws && this.currentStatus !== "connected") {
        ws.close();
      }
    }, this.connectTimeoutMs);

    this.ws.onopen = () => {
      this.reconnectDelay = 800;
      // TCP upgrade succeeded; arm the liveness probe. Probe pings only run
      // once the connect handshake completes (status === "connected").
      this.startHeartbeat();
      this.sendRequest("connect", {
        protocol_version: 1,
        client: { id: "web", version: "1.0.0" },
        device: { id: this.deviceId },
        scopes: ["chat", "read", "write"],
        // Mobile gateway requires the per-install token at the handshake.
        ...(this.gatewayToken ? { auth: { token: this.gatewayToken } } : {}),
      });
    };

    this.ws.onmessage = (event) => {
      try {
        const msg = JSON.parse(event.data);
        if (msg.type === "res") {
          // Resolve response waiters
          const waiter = this.responseWaiters.get(msg.id);
          if (waiter) {
            this.responseWaiters.delete(msg.id);
            if (msg.ok) {
              waiter.resolve(msg.payload);
            } else {
              waiter.reject(
                new Error(msg.error?.message || "Request failed")
              );
            }
          }
          if (msg.ok && msg.payload?.protocol_version) {
            this.clearConnectTimeout();
            this.setStatus("connected");
            this.serverInfo = {
              version: msg.payload.server?.version,
              conn_id: msg.payload.server?.conn_id,
              features: msg.payload.features,
              scopes_granted: msg.payload.scopes_granted,
            };
            if (this.subscribedSessions.length > 0) {
              this.sendRequest("sessions.subscribe", {
                session_ids: this.subscribedSessions,
              });
            }
          }
        }
        if (msg.type === "event") {
          const evt = msg as WsEvent;
          if (evt.event === "session.created") {
            const sid = evt.payload?.session_id as string;
            if (sid && !this.subscribedSessions.includes(sid)) {
              this.subscribedSessions.push(sid);
            }
          }
          this.listeners.forEach((cb) => cb(evt));
          // Route event to session-specific queue for session-aware processing
          const sid = this.getSessionIdFromEventPayload(evt);
          if (sid) {
            this.routeToSessionQueue(sid, evt);
          }
        }
      } catch {
        /* ignore malformed */
      }
    };

    this.ws.onclose = () => {
      this.stopHeartbeat();
      this.clearConnectTimeout();
      this.setStatus("disconnected");
      this.scheduleReconnect();
    };

    this.ws.onerror = () => {
      this.stopHeartbeat();
      this.clearConnectTimeout();
      this.setStatus("disconnected");
      this.scheduleReconnect();
    };
  }

  private scheduleReconnect() {
    if (this.reconnectTimer) return;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, this.reconnectDelay);
    this.reconnectDelay = Math.min(
      this.reconnectDelay * this.reconnectMultiplier,
      this.reconnectCap
    );
  }

  private clearConnectTimeout() {
    if (this.connectTimeoutTimer) {
      clearTimeout(this.connectTimeoutTimer);
      this.connectTimeoutTimer = null;
    }
  }

  private stopHeartbeat() {
    if (this.heartbeatTimer) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
  }

  private startHeartbeat() {
    this.stopHeartbeat();
    this.heartbeatTimer = setInterval(
      () => this.probeLiveness(),
      this.heartbeatIntervalMs
    );
  }

  /** Probe the gateway with a ping; force-close if it never answers. */
  private probeLiveness() {
    const ws = this.ws;
    if (
      !ws ||
      ws.readyState !== WebSocket.OPEN ||
      this.currentStatus !== "connected"
    ) {
      return;
    }
    this.sendRequestAndWait("ping", {}, this.heartbeatTimeoutMs).catch(() => {
      ws.close();
    });
  }

  private sendRequest(
    method: string,
    params?: Record<string, unknown>
  ): string {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return "";
    const id = "req_" + ++this.reqId;
    this.ws.send(JSON.stringify({ type: "req", id, method, params }));
    return id;
  }

  /** True while the New Session welcome page is showing (no session yet). */
  isPendingNewSession(): boolean {
    return this.pendingNewSession;
  }

  /** Arm the welcome page: clear the in-memory view without touching any
   *  session. The real session is created on the first send (see run()).
   *  An optional agentId binds the summoned agent to that future session —
   *  clicking an agent in the sidebar navigates here instead of creating
   *  an empty session up front. */
  armNewSession(agentId?: string): void {
    this.pendingNewSession = true;
    this.pendingAgentId = agentId;
    this.setMessages([]);
  }

  createSession(agentId?: string): string {
    this.pendingNewSession = false;
    this.pendingAgentId = undefined;
    // Reuse an existing empty session if one exists
    const sessions = this.getLocalSessions();
    for (const sid of sessions) {
      const history = this.getHistory(sid);
      if (history.length === 0) {
        this.sessionId = sid;
        localStorage.setItem("syscity_session", this.sessionId);
        this.subscribedSessions = [];
        this.sendRequest("sessions.create", {
          session_id: this.sessionId,
          agent_id: agentId,
        });
        this.notifySessionChange();
        return this.sessionId;
      }
    }

    const newSessionId = `web:${this.deviceId}_${Date.now()}`;
    this.sessionId = newSessionId;
    localStorage.setItem("syscity_session", this.sessionId);
    this.subscribedSessions = [];
    this.sendRequest("sessions.create", {
      session_id: this.sessionId,
      agent_id: agentId,
    });
    // Persist session in local list
    if (!sessions.includes(newSessionId)) {
      sessions.unshift(newSessionId);
      localStorage.setItem("syscity_sessions", JSON.stringify(sessions));
    }
    this.notifySessionChange();
    return this.sessionId;
  }

  async listAgentRegistry(): Promise<
    Array<{
      id: string;
      display_name: string;
      emoji: string;
      is_valid: boolean;
      has_heartbeat: boolean;
    }>
  > {
    try {
      await this.waitForConnected(8000);
      const res = (await this.sendRequestAndWait("agents.registry", {})) as
        | {
            agents?: Array<{
              id: string;
              display_name: string;
              emoji?: string;
              is_valid: boolean;
              has_heartbeat: boolean;
            }>;
            count?: number;
          }
        | undefined;
      return (res?.agents || []).map((a) => ({
        ...a,
        emoji: a.emoji || "🤖",
      }));
    } catch {
      return [];
    }
  }

  /** List one directory level of an agent's workspace (undefined = default agent). */

  /** Read a text file from an agent's workspace (undefined = default agent). */

  getLocalSessions(): string[] {
    try {
      return JSON.parse(localStorage.getItem("syscity_sessions") || "[]");
    } catch {
      return [];
    }
  }

  async listSessions(): Promise<
    Array<{
      id: string;
      label?: string;
      agent_id?: string;
      pinned?: boolean;
      model?: string | null;
      last_activity?: number;
    }>
  > {
    const local = this.getLocalSessions();
    if (this.currentStatus !== "connected") {
      return local.map((id) => ({ id, label: this.sessionLabel(id) }));
    }
    try {
      const payload = (await this.sendRequestAndWait("sessions.list", {})) as
        | {
            sessions?: Array<{
              session_id: string;
              name?: string;
              agent_id?: string;
              pinned?: boolean;
              model?: string | null;
              last_activity?: string;
            }>;
          }
        | undefined;
      const remote = payload?.sessions || [];
      const merged = new Map<
        string,
        {
          label: string;
          agent_id?: string;
          pinned?: boolean;
          model?: string | null;
          last_activity?: number;
        }
      >();
      for (const s of remote) {
        merged.set(s.session_id, {
          label: s.name || this.sessionLabel(s.session_id),
          agent_id: s.agent_id,
          pinned: s.pinned,
          model: s.model ?? null,
          last_activity: s.last_activity
            ? new Date(s.last_activity).getTime()
            : undefined,
        });
      }
      for (const id of local) {
        if (!merged.has(id)) {
          merged.set(id, { label: this.sessionLabel(id) });
        }
      }
      return Array.from(merged.entries()).map(([id, data]) => ({
        id,
        label: data.label,
        agent_id: data.agent_id,
        pinned: data.pinned,
        model: data.model ?? null,
        last_activity: data.last_activity,
      }));
    } catch {
      return local.map((id) => ({ id, label: this.sessionLabel(id) }));
    }
  }




  /** Answer a pending `ask_user` question, waking the blocked agent turn. */

  async deleteSession(sessionId: string): Promise<boolean> {
    try {
      await this.sendRequestAndWait("sessions.delete", { session_id: sessionId });
      // Clean up localStorage
      const local = this.getLocalSessions().filter((id) => id !== sessionId);
      localStorage.setItem("syscity_sessions", JSON.stringify(local));
      this.clearHistory(sessionId);
      if (this.sessionId === sessionId) {
        this.sessionId = `web:${this.deviceId}_${Date.now()}`;
        localStorage.setItem("syscity_session", this.sessionId);
        this.setMessages([]);
        this.notifySessionChange();
      }
      return true;
    } catch {
      return false;
    }
  }

  private sessionLabel(id: string): string {
    const parts = id.split("_");
    const last = parts[parts.length - 1];
    if (/^\d+$/.test(last)) {
      const d = new Date(parseInt(last));
      if (!isNaN(d.getTime())) {
        return d.toLocaleString(undefined, {
          month: "short",
          day: "numeric",
          hour: "2-digit",
          minute: "2-digit",
        });
      }
    }
    return id.slice(0, 20);
  }

  private waitForConnected(timeout = 8000): Promise<void> {
    return new Promise((resolve, reject) => {
      if (this.currentStatus === "connected") {
        resolve();
        return;
      }
      const unsub = this.onStatusChange((status) => {
        if (status === "connected") {
          unsub();
          resolve();
        }
      });
      setTimeout(() => {
        unsub();
        reject(new Error("WebSocket connection timeout"));
      }, timeout);
    });
  }

  sendRequestAndWait(
    method: string,
    params?: Record<string, unknown>,
    timeoutMs = 5000
  ): Promise<unknown> {
    return new Promise((resolve, reject) => {
      const id = this.sendRequest(method, params);
      if (!id) {
        reject(new Error("WebSocket not connected"));
        return;
      }
      this.responseWaiters.set(id, { resolve, reject });
      setTimeout(() => {
        if (this.responseWaiters.has(id)) {
          this.responseWaiters.delete(id);
          reject(new Error("Request timeout"));
        }
      }, timeoutMs);
    });
  }

  /** True when running inside a Tauri WebView (desktop or mobile). */
  isTauri(): boolean {
    return typeof window !== "undefined" && "__TAURI__" in window;
  }

  switchSession(sessionId: string): void {
    this.pendingNewSession = false;
    this.sessionId = sessionId;
    localStorage.setItem("syscity_session", this.sessionId);
    if (!this.subscribedSessions.includes(sessionId)) {
      this.subscribedSessions.push(sessionId);
      this.sendRequest("sessions.subscribe", {
        session_ids: [sessionId],
      });
    }
    this.notifySessionChange();
  }

  /* ── Event routing helpers ── */
  /** Session-scoped event names that carry session_id in payload. */
  private static SESSION_EVENTS = new Set([
    'chat.delta', 'chat.final', 'chat.error',
    'agent.thinking', 'tool.calling', 'tool.result',
    'cron.completed', 'goal.progress',
    'session.created', 'session.renamed', 'session.pinned',
  ]);

  /** Extract session_id from event payload. Returns '' for non-session events. */
  private getSessionIdFromEventPayload(evt: WsEvent): string {
    if (evt.payload?.session_id && SyscityWebSocketTransport.SESSION_EVENTS.has(evt.event)) {
      return String(evt.payload.session_id);
    }
    return '';
  }

  /** Route an event to a session-specific queue, waking a waiter if available. */
  private routeToSessionQueue(sessionId: string, evt: WsEvent): void {
    if (!this.eventQueues.has(sessionId)) {
      this.eventQueues.set(sessionId, []);
    }
    const queue = this.eventQueues.get(sessionId)!;
    const waiters = this.eventWaiters.get(sessionId);
    if (waiters && waiters.length > 0) {
      const waiter = waiters.shift()!;
      waiter(evt);
    } else {
      queue.push(evt);
    }
  }

  /* ── Session history (localStorage) ── */
  private historyKey(sessionId: string): string {
    return `syscity_history_${sessionId}`;
  }

  saveMessage(msg: ChatMessage): void {
    const key = this.historyKey(this.sessionId);
    const history: ChatMessage[] = JSON.parse(localStorage.getItem(key) || "[]");
    history.push({
      ...msg,
      timestamp: msg.timestamp ?? Date.now(),
    });
    // Keep last 200 messages
    if (history.length > 200) history.splice(0, history.length - 200);
    localStorage.setItem(key, JSON.stringify(history));
  }

  /** Merge document-ref parts from localStorage into API-sourced messages. */
  private enrichWithDocumentRefs(api: ChatMessage[], local: ChatMessage[]): void {
    const localById = new Map(local.map((m) => [m.id, m]));
    for (const msg of api) {
      const localMsg = localById.get(msg.id);
      if (!localMsg?.parts) continue;
      const docRefs = localMsg.parts.filter((p) => p.type === "document-ref");
      if (docRefs.length === 0) continue;
      // Skip if message already has document-ref (reconstructed from tool args)
      if (msg.parts?.some((p) => p.type === "document-ref")) continue;
      if (msg.parts) {
        // Insert before the first text part (preserves order: reasoning → tool-calls → doc-ref → text)
        const textIdx = msg.parts.findIndex((p) => p.type === "text");
        if (textIdx >= 0) {
          msg.parts.splice(textIdx, 0, ...docRefs);
        } else {
          msg.parts.push(...docRefs);
        }
      } else {
        msg.parts = [...docRefs];
      }
    }
  }

  getHistory(sessionId: string): ChatMessage[] {
    try {
      return JSON.parse(localStorage.getItem(this.historyKey(sessionId)) || "[]");
    } catch {
      return [];
    }
  }

  private parseHistoryMessages(
    raw: Array<{
      id: string;
      role: string;
      content: string;
      reasoning_content?: string;
      tool_calls?: Array<{
        id: string;
        call_type: string;
        function: { name: string; arguments: string };
        result?: string;
      }>;
      timestamp: number;
      duration_ms?: number;
      tool_count?: number;
      turn_id?: string | null;
    }>
  ): ChatMessage[] {
    return raw.map((m) => {
      const parts: ChatMessagePart[] = [];
      // Build parts: reasoning → tool calls → text
      if (m.reasoning_content) {
        parts.push({ type: "reasoning", text: m.reasoning_content });
      }
      if (m.tool_calls && m.tool_calls.length > 0) {
        for (const tc of m.tool_calls) {
          let args: Record<string, unknown> = {};
          try {
            args = JSON.parse(tc.function.arguments);
          } catch {
            args = { raw: tc.function.arguments };
          }
          parts.push({
            type: "tool-call",
            toolName: tc.function.name,
            args,
            result: tc.result,
          });
          // Reconstruct document-ref part from write_report tool arguments
          // Also match old name "write_document" for backward compat with saved sessions
          if ((tc.function.name === "write_report" || tc.function.name === "write_document") && args.filename) {
            // Saved sessions persist only the tool args, not the result data,
            // so the owner-addressed url is reconstructed. write_report writes
            // the default agent's artifacts to @default/<filename>.
            const fmt = (args.format as string) || "markdown";
            const url = `/api/v1/artifacts/@default/${args.filename}`;
            const exportTarget =
              fmt === "slides" ? "pptx" : fmt === "docx" ? "docx" : fmt === "xlsx" ? "xlsx" : null;
            parts.push({
              type: "document-ref",
              data: {
                filename: args.filename,
                title: args.title || (args.filename as string),
                format: fmt,
                url,
                export_url: exportTarget ? `${url}?to=${exportTarget}` : undefined,
              },
            } as ChatMessagePart);
          }
        }
      }
      if (m.content) {
        parts.push({ type: "text", text: m.content });
      }
      return {
        id: m.id,
        role: m.role as "user" | "assistant",
        content: m.content,
        parts: parts.length > 0 ? parts : undefined,
        timestamp: m.timestamp,
        durationMs: m.duration_ms,
        toolCount: m.tool_count,
        turnId: m.turn_id || undefined,
      };
    });
  }

  async loadHistory(sessionId: string): Promise<{
    messages: ChatMessage[];
    hasMore: boolean;
  }> {
    try {
      // Wait for WebSocket to connect before requesting history
      // so we get the full backend data (reasoning + tool_calls)
      // instead of falling back to the text-only localStorage copy.
      await this.waitForConnected(8000);
      const res = (await this.sendRequestAndWait("chat.history", {
        session_id: sessionId,
        limit: 100,
      })) as
        | {
            messages?: Array<{
              id: string;
              role: string;
              content: string;
              reasoning_content?: string;
              tool_calls?: Array<{
                id: string;
                call_type: string;
                function: { name: string; arguments: string };
                result?: string;
              }>;
              timestamp: number;
              duration_ms?: number;
              tool_count?: number;
            }>;
            has_more?: boolean;
          }
        | undefined;
      const msgs = this.parseHistoryMessages(res?.messages || []);
      // Enrich API messages with document-ref parts from localStorage
      // (the backend doesn't know about these frontend-only parts)
      try {
        this.enrichWithDocumentRefs(msgs, this.getHistory(sessionId));
      } catch { /* ignore merge errors */ }
      return {
        messages: msgs,
        hasMore: res?.has_more ?? msgs.length === 100,
      };
    } catch {
      // Fallback to localStorage (already stores full ChatMessage with parts)
      const msgs = this.getHistory(sessionId);
      return { messages: msgs, hasMore: false };
    }
  }

  async loadMoreHistory(
    sessionId: string,
    before: number
  ): Promise<{ messages: ChatMessage[]; hasMore: boolean }> {
    try {
      await this.waitForConnected(8000);
      const res = (await this.sendRequestAndWait("chat.history", {
        session_id: sessionId,
        limit: 100,
        before,
      })) as
        | {
            messages?: Array<{
              id: string;
              role: string;
              content: string;
              reasoning_content?: string;
              tool_calls?: Array<{
                id: string;
                call_type: string;
                function: { name: string; arguments: string };
                result?: string;
              }>;
              timestamp: number;
              duration_ms?: number;
              tool_count?: number;
            }>;
            has_more?: boolean;
          }
        | undefined;
      const msgs = this.parseHistoryMessages(res?.messages || []);
      try {
        this.enrichWithDocumentRefs(msgs, this.getHistory(sessionId));
      } catch { /* ignore merge errors */ }
      return {
        messages: msgs,
        hasMore: res?.has_more ?? msgs.length === 100,
      };
    } catch {
      return { messages: [], hasMore: false };
    }
  }

  clearHistory(sessionId: string): void {
    localStorage.removeItem(this.historyKey(sessionId));
  }

  /** Replace the history for a session and persist it to localStorage. */
  setHistory(sessionId: string, messages: ChatMessage[]): void {
    this.saveHistory(sessionId, messages);
    if (sessionId === this.sessionId) {
      this.setMessages(messages);
    }
  }

  saveHistory(sessionId: string, messages: ChatMessage[]): void {
    const key = this.historyKey(sessionId);
    const trimmed = messages.slice(-200).map((m) => ({
      ...m,
      timestamp: m.timestamp ?? Date.now(),
    }));
    localStorage.setItem(key, JSON.stringify(trimmed));
  }

  /** Edit a user message: remove it and all following messages, then re-send. */
  async editUserMessage(messageId: string, newText: string): Promise<void> {
    const currentMessages = this.messages;
    const idx = currentMessages.findIndex((m) => m.id === messageId);
    if (idx === -1) return;

    const kept = currentMessages.slice(0, idx);
    const edited: ChatMessage = {
      id: messageId,
      role: "user",
      content: newText,
      timestamp: Date.now(),
    };
    const next = [...kept, edited];
    this.setHistory(this.sessionId, next);

    // Trigger assistant-ui to run again with the edited prompt
    await this.resendLastUserMessage();
  }

  private pendingMessageKey(sessionId: string): string {
    return `syscity_pending_msg_${sessionId}`;
  }

  /** Save a pending user message text for a session (message that was sent but
   *  didn't get a complete AI response before session switch). */
  savePendingMessage(sessionId: string, text: string): void {
    if (text) {
      localStorage.setItem(this.pendingMessageKey(sessionId), text);
    } else {
      localStorage.removeItem(this.pendingMessageKey(sessionId));
    }
  }

  /** Retrieve and clear the pending message text for a session. */
  getPendingMessage(sessionId: string): string | null {
    const key = this.pendingMessageKey(sessionId);
    const text = localStorage.getItem(key);
    localStorage.removeItem(key);
    return text;
  }

  /** Remove the last assistant reply and re-send the preceding user message. */
  async regenerateAssistantMessage(messageId: string): Promise<void> {
    const currentMessages = this.messages;
    const idx = currentMessages.findIndex((m) => m.id === messageId);
    if (idx === -1) return;

    const before = currentMessages.slice(0, idx);
    this.setHistory(this.sessionId, before);

    await this.resendLastUserMessage();
  }

  private async resendLastUserMessage(): Promise<void> {
    const last = this.messages[this.messages.length - 1];
    if (!last || last.role !== "user") return;

    // Abort any in-flight generation
    this.abortControllers.get(this.sessionId)?.abort();
    this.runningSessions.delete(this.sessionId);
    this.runListeners.forEach((cb) => cb(false));
    this.notifyRunningSessionsChanged();

    // Send via chat.send like a normal user turn. The assistant-ui runtime
    // drives the streaming response via run() when messages change.
    this.sendRequest("chat.send", {
      session_id: this.sessionId,
      message: last.content,
    });
  }

  /* ── Config ── */


  /* ── Feedback ── */

  /**
   * Submit a Like/Dislike vote for a turn.
   *
   * `input`/`response` are optional and only used to seed a `human:dislike`
   * pending badcase on a down vote; the vote itself only needs the `turnId`.
   */

  /* ── Eval dashboard (§八 评测看板) ── */

  /** Fetch the read-only eval dashboard aggregate. Degrades to all-zero
   *  buckets when the method errors or the stores are missing. */


  /* ── Feedback ops (rule-based, no-LLM aggregation) ── */

  /** Fetch the read-only `feedback.ops` aggregation report. Degrades to an
   *  all-zero/empty report when the method errors or the stores are missing. */


  /* ── Onboarding (first-launch identity wizard) ── */

  /** Query the identity wizard status (WS `onboarding.status`). */
  async onboardingStatus(): Promise<OnboardingStatus> {
    await this.waitForConnected(8000);
    const res = (await this.sendRequestAndWait("onboarding.status", {})) as {
      status?: string;
    };
    return { status: res.status === "done" ? "done" : "pending" };
  }

  /** Submit the identity wizard form (WS `onboarding.apply`). */
  async applyOnboarding(
    payload: OnboardingPayload
  ): Promise<{ ok: boolean; error?: string }> {
    try {
      await this.waitForConnected(8000);
      await this.sendRequestAndWait("onboarding.apply", { ...payload });
      return { ok: true };
    } catch (e) {
      return { ok: false, error: e instanceof Error ? e.message : String(e) };
    }
  }

  // ── Cloud (WS admin methods) ────────────────────────────────────────────

  /** Cloud status block (WS `cloud.status`). */

  /** Plan + credit balance (WS `cloud.subscription`). */

  /** Credit usage for the last `days` (WS `cloud.usage`). */

  /** Persist a cloud session token (WS `cloud.token`). */

  /** Forget the cloud session (WS `cloud.logout`). */

  // ── Update (WS admin methods) ───────────────────────────────────────────

  /** Latest release info (WS `update.status`). */

  /** Current update progress (WS `update.progress`). */

  /** Trigger the self-update (WS `update.trigger`). */

  // ── Marketplace catalog (WS `connectors.catalog`) ───────────────────────

  /** The marketplace catalog document (WS `connectors.catalog`). */







  /* ── Model operations ── */





  /* ── MCP operations ── */

  /** Fetch MCP server presets from ~/.syscity/mcp.toml */







  /* ── Permissions ── */

  /* ── Log streaming ── */
  subscribeLogs(): void {
    this.sendRequest("logs.subscribe", {});
  }

  unsubscribeLogs(): void {
    this.sendRequest("logs.unsubscribe", {});
  }

  /* ── Channel operations ── */




  /* ── Device operations (mobile §4.1/§4.2/§4.5) ── */
  /**
   * List device capabilities with grant state.
   * Returns `null` when the platform is unsupported (e.g. desktop / web).
   */

  /** Report a runtime permission's grant state. `null` on unsupported platform. */

  /** Ask the user to grant a runtime permission. May take up to ~60 s. */

  /** Report loopback adb pairing status. `null` on unsupported platform. */

  /**
   * Pair with the phone's own wireless-debugging adb server (§4.5).
   * `port` is the pairing-port shown in the "Pair device with pairing code"
   * dialog; `connectPort` (defaults to `port`) is the connect target shown on
   * the wireless-debugging screen. May take up to ~60 s.
   */

  /* ── Shortcuts / AppIntents bus (iOS §4.6) ── */
  /**
   * Run an iOS Shortcut by name, optionally passing text input (§4.6).
   * The shortcut runs in the Shortcuts app (foreground hand-off). Returns
   * `{launched}` — `null` on unsupported platforms.
   */

  /**
   * List + consume outputs returned by the SyscityOutput AppIntent (§4.6).
   * Reading consumes the result (delete-read). `null` on unsupported platform.
   */

  /** List + consume prompts sent via the AskSyscity AppIntent (§4.6). */

  /* ── In-memory message state for UI ── */
  getServerInfo(): { version?: string; conn_id?: string; features?: string[]; scopes_granted?: string[] } {
    return this.serverInfo;
  }

  getGatewayUrl(): string {
    return this.gatewayUrl;
  }

  getMessages(sessionId?: string): ChatMessage[] {
    const sid = sessionId ?? this.sessionId;
    if (sid === this.sessionId) {
      return this.messages;
    }
    return this.messagesMap.get(sid) || [];
  }

  setMessages(msgs: ChatMessage[], sessionId?: string): void {
    const sid = sessionId ?? this.sessionId;
    const seen = new Set<string>();
    const deduped: ChatMessage[] = [];
    for (const m of msgs) {
      if (!seen.has(m.id)) {
        seen.add(m.id);
        deduped.push(m);
      }
    }
    if (sid === this.sessionId) {
      this.messages = deduped;
    } else {
      this.messagesMap.set(sid, deduped);
    }
    this.messagesListeners.forEach((cb) => cb(this.messages));
  }

  /** Save a snapshot of session messages for seamless switch-back restoration. */
  saveSessionMessages(sessionId: string, msgs: ChatMessage[]): void {
    this.messagesMap.set(sessionId, [...msgs]);
  }

  /** Get saved messages for any session (not necessarily current). */
  getSessionMessages(sessionId: string): ChatMessage[] {
    return this.messagesMap.get(sessionId) || [];
  }

  onMessagesChange(callback: MessagesCallback): () => void {
    this.messagesListeners.add(callback);
    callback(this.messages);
    return () => this.messagesListeners.delete(callback);
  }

  async *run(
    options: ChatModelRunOptions
  ): AsyncGenerator<ChatModelRunResult, void> {
    const { messages, abortSignal } = options;
    const last = messages[messages.length - 1];
    if (!last || last.role !== "user") {
      return;
    }

    const text = last.content
      .map((c) => (c.type === "text" ? c.text : ""))
      .join("");

    // Deferred session creation: the New Session welcome page arms this flag;
    // create the real session now, before anything is persisted or sent —
    // saveMessage() below writes to this.sessionId's history key, and the
    // gateway dispatches frames sequentially so sessions.create lands before
    // chat.send.
    if (this.pendingNewSession) {
      this.pendingNewSession = false;
      this.createSession(this.pendingAgentId);
      this.pendingAgentId = undefined;
    }

    const startTime = Date.now();

    const userMsg: ChatMessage = {
      id: `u_${Date.now()}`,
      role: "user",
      content: text,
    };
    // Save user message to history
    this.saveMessage(userMsg);
    this.messages = [...this.messages, userMsg];
    this.messagesListeners.forEach((cb) => cb(this.messages));

    // ── Slash command interception ──
    const parsed = parseCommand(text);
    if (parsed) {
      const cmd = findCommand(parsed.command);
      if (cmd) {
        // Local commands: handled client-side without RPC
        if (LOCAL_COMMANDS.has(parsed.command)) {
          if (parsed.command === "new") {
            // Same deferred flow as the sidebar New Session button: show the
            // welcome page; the next send creates the real session.
            this.armNewSession();
            return;
          }
          if (parsed.command === "clear") {
            this.clearHistory(this.sessionId);
            this.setMessages([]);
            const cmdDuration = Date.now() - startTime;
            const assistantMsg: ChatMessage = {
              id: `a_${Date.now()}`,
              role: "assistant",
              content: "History cleared.",
              durationMs: cmdDuration,
              toolCount: 0,
            };
            this.saveMessage(assistantMsg);
            this.messages = [...this.messages, assistantMsg];
            this.messagesListeners.forEach((cb) => cb(this.messages));
            yield {
              content: [makeTextPart("History cleared.")],
              status: { type: "complete", reason: "stop" },
            };
            return;
          }
        }

        // Remote commands: send via RPC and yield response as assistant message
        try {
          await this.waitForConnected(5000);
          const result = (await this.sendRequestAndWait("commands.execute", {
            command: parsed.command,
            args: parsed.args,
            session_id: this.sessionId,
          })) as { text?: string } | undefined;

          const responseText = result?.text ?? "Command executed.";
          const cmdDuration = Date.now() - startTime;
          const assistantMsg: ChatMessage = {
            id: `a_${Date.now()}`,
            role: "assistant",
            content: responseText,
            durationMs: cmdDuration,
            toolCount: 0,
          };
          this.saveMessage(assistantMsg);
          this.messages = [...this.messages, assistantMsg];
          this.messagesListeners.forEach((cb) => cb(this.messages));
          yield {
            content: [makeTextPart(responseText)],
            status: { type: "complete", reason: "stop" },
          };
          return;
        } catch (err) {
          const errorText = `Command error: ${err instanceof Error ? err.message : String(err)}`;
          const cmdDuration = Date.now() - startTime;
          const assistantMsg: ChatMessage = {
            id: `a_${Date.now()}`,
            role: "assistant",
            content: errorText,
            durationMs: cmdDuration,
            toolCount: 0,
          };
          this.saveMessage(assistantMsg);
          this.messages = [...this.messages, assistantMsg];
          this.messagesListeners.forEach((cb) => cb(this.messages));
          yield {
            content: [makeTextPart(errorText)],
            status: { type: "complete", reason: "stop" },
          };
          return;
        }
      }

      // Unrecognized command: show error
      const errorText = `Unknown command: /${parsed.command}`;
      const cmdDuration = Date.now() - startTime;
      const assistantMsg: ChatMessage = {
        id: `a_${Date.now()}`,
        role: "assistant",
        content: errorText,
        durationMs: cmdDuration,
        toolCount: 0,
      };
      this.saveMessage(assistantMsg);
      this.messages = [...this.messages, assistantMsg];
      this.messagesListeners.forEach((cb) => cb(this.messages));
      yield {
        content: [makeTextPart(errorText)],
        status: { type: "complete", reason: "stop" },
      };
      return;
    }

    const sessionId = this.sessionId;
    this.runningSessions.add(sessionId);
    this.abortControllers.set(sessionId, new AbortController());
    this.notifyRunningSessionsChanged();

    this.sendRequest("chat.send", {
      session_id: sessionId,
      message: text,
    });

    const parts: (
      | TextMessagePart
      | ReasoningMessagePart
      | ToolCallMessagePart
    )[] = [];
    let currentText = "";
    let currentReasoning = "";
    let toolCalls = new Map<string, ToolCallMessagePart>();
    let aiMsgId = `a_${Date.now()}`;
    let hasShownThinking = false;
    const extraParts: any[] = [];

    // Add empty AI message for streaming updates
    const aiMsg: ChatMessage = {
      id: aiMsgId,
      role: "assistant",
      content: "",
      parts: [],
    };
    this.messages = [...this.messages, aiMsg];
    this.messagesListeners.forEach((cb) => cb(this.messages));

    // Show "Thinking..." placeholder immediately while waiting for first event
    aiMsg.parts = [{ type: "reasoning", text: "" }];
    aiMsg.liveStatus = { status: "thinking" };
    hasShownThinking = true;
    this.messagesListeners.forEach((cb) => cb(this.messages));
    yield { content: [makeReasoningPart("")] };

    try {
      while (true) {
        const signal = this.abortControllers.get(sessionId)?.signal ?? abortSignal;
        const evt = await this.nextEvent(sessionId, signal);
        if (!evt) {
          break;
        }

        switch (evt.event) {
          case "chat.delta": {
            const delta = (evt.payload?.delta as string) || (evt.payload?.content as string) || "";
            currentText += delta;
            // Rebuild parts: reasoning (if any) + text + tool calls
            const newParts: typeof parts = [];
            if (currentReasoning) {
              newParts.push(makeReasoningPart(currentReasoning));
            }
            for (const tc of toolCalls.values()) {
              newParts.push(tc);
            }
            newParts.push(makeTextPart(currentText));
            parts.length = 0;
            parts.push(...newParts);
            // Update in-memory AI message
            aiMsg.content = currentText;
            aiMsg.parts = newParts.map(toChatPart);
            this.messagesListeners.forEach((cb) => cb(this.messages));
            yield { content: [...parts] };
            break;
          }
          case "agent.thinking": {
            const thinking = (evt.payload?.content as string) || "";
            currentReasoning += thinking;
            const newParts: typeof parts = [];
            if (currentReasoning) {
              newParts.push(makeReasoningPart(currentReasoning));
            }
            for (const tc of toolCalls.values()) {
              newParts.push(tc);
            }
            if (currentText) {
              newParts.push(makeTextPart(currentText));
            }
            parts.length = 0;
            parts.push(...newParts);
            aiMsg.content = currentText;
            aiMsg.parts = newParts.map(toChatPart);
            // Only show thinking status on the first occurrence
            if (!hasShownThinking) {
              aiMsg.liveStatus = { status: "thinking" };
              hasShownThinking = true;
            }
            this.messagesListeners.forEach((cb) => cb(this.messages));
            yield { content: [...parts] };
            break;
          }
          case "tool.calling": {
            const toolName = (evt.payload?.tool_name as string) || "tool";
            const rawArgs = evt.payload?.arguments;
            let toolArgs: Record<string, unknown> = {};
            if (typeof rawArgs === "string") {
              try {
                toolArgs = JSON.parse(rawArgs) as Record<string, unknown>;
              } catch {
                toolArgs = { raw: rawArgs };
              }
            } else if (rawArgs && typeof rawArgs === "object") {
              toolArgs = rawArgs as Record<string, unknown>;
            }
            const toolCallId = `tc_${toolCalls.size}_${Date.now()}`;
            const tc = makeToolCallPart(toolCallId, toolName, toolArgs);
            toolCalls.set(toolCallId, tc);
            const newParts: typeof parts = [];
            if (currentReasoning) {
              newParts.push(makeReasoningPart(currentReasoning));
            }
            for (const t of toolCalls.values()) {
              newParts.push(t);
            }
            if (currentText) {
              newParts.push(makeTextPart(currentText));
            }
            parts.length = 0;
            parts.push(...newParts);
            aiMsg.content = currentText;
            aiMsg.parts = newParts.map(toChatPart);
            aiMsg.liveStatus = { status: "tool_calling", toolName };
            this.messagesListeners.forEach((cb) => cb(this.messages));
            yield { content: [...parts] };
            break;
          }
          case "tool.result": {
            const toolName = (evt.payload?.tool_name as string) || "";
            const result = evt.payload?.result;
            const data = evt.payload?.data;
            let matched = false;
            // Find matching tool call by name and update with result
            for (const [id, tc] of toolCalls) {
              if (tc.toolName === toolName && tc.result === undefined) {
                const updated = makeToolCallPart(
                  id,
                  toolName,
                  tc.args,
                  result,
                  data
                );
                toolCalls.set(id, updated);
                matched = true;
                break;
              }
            }
            if (!matched) {
              console.warn("Tool result received but no matching tool call found:", toolName);
            }
            const newParts: typeof parts = [];
            if (currentReasoning) {
              newParts.push(makeReasoningPart(currentReasoning));
            }
            for (const t of toolCalls.values()) {
              newParts.push(t);
            }
            // Add document-ref part for write_report tool results
            // Also match old name "write_document" for backward compat
            if ((toolName === "write_report" || toolName === "write_document") && data && typeof data === "object" && "filename" in (data as any)) {
              const d = data as { filename: string; title?: string; format?: string; url?: string; export_url?: string };
              const docPart: any = {
                type: "document-ref",
                data: {
                  filename: d.filename,
                  title: d.title || d.filename,
                  format: d.format || "markdown",
                  url: d.url,
                  export_url: d.export_url,
                },
              };
              newParts.push(docPart);
              extraParts.push(docPart);
            }
            if (currentText) {
              newParts.push(makeTextPart(currentText));
            }
            parts.length = 0;
            parts.push(...newParts);
            aiMsg.content = currentText;
            aiMsg.parts = newParts.map(toChatPart);
            // Keep liveStatus visible until chat.final so the user sees
            // the tool is still part of the ongoing turn.
            this.messagesListeners.forEach((cb) => cb(this.messages));
            yield { content: [...parts] };
            break;
          }
          case "chat.final": {
            const response =
              (evt.payload?.response as string) || currentText;
            currentText = response;
            const finalParts: typeof parts = [];
            if (currentReasoning) {
              finalParts.push(makeReasoningPart(currentReasoning));
            }
            for (const t of toolCalls.values()) {
              finalParts.push(t);
            }
            finalParts.push(...extraParts);
            extraParts.length = 0;
            finalParts.push(makeTextPart(currentText));
            const durationMs = Date.now() - startTime;
            const toolCount = toolCalls.size;
            const turnId = (evt.payload?.turn_id as string) || undefined;
            // Save final AI message to history with full parts and metadata
            this.saveMessage({ ...aiMsg, durationMs, toolCount, turnId });
            aiMsg.content = currentText;
            aiMsg.parts = finalParts.map(toChatPart);
            aiMsg.durationMs = durationMs;
            aiMsg.toolCount = toolCount;
            aiMsg.turnId = turnId;
            aiMsg.liveStatus = undefined;
            this.messagesListeners.forEach((cb) => cb(this.messages));
            yield { content: finalParts, status: { type: "complete", reason: "stop" } };
            return;
          }
          case "chat.error": {
            throw new Error(
              (evt.payload?.message as string) || "Chat error"
            );
          }
        }
      }
    } catch (err) {
      // On error, clear live status and append an error indicator
      aiMsg.liveStatus = undefined;
      aiMsg.content = currentText || "Error occurred";
      aiMsg.durationMs = Date.now() - startTime;
      aiMsg.toolCount = toolCalls.size;
      this.messagesListeners.forEach((cb) => cb(this.messages));
      throw err;
    } finally {
      // Safety net: always clear live status
      if (aiMsg.durationMs === undefined) {
        aiMsg.durationMs = Date.now() - startTime;
        aiMsg.toolCount = toolCalls.size;
      }
      aiMsg.liveStatus = undefined;
      this.runningSessions.delete(sessionId);
      this.abortControllers.delete(sessionId);
      this.runListeners.forEach((cb) => cb(false));
      this.notifyRunningSessionsChanged();
    }
  }

  private nextEvent(sessionId: string, abortSignal?: AbortSignal): Promise<WsEvent | null> {
    return new Promise((resolve) => {
      if (!this.eventQueues.has(sessionId)) {
        this.eventQueues.set(sessionId, []);
      }
      const queue = this.eventQueues.get(sessionId)!;
      if (queue.length > 0) {
        resolve(queue.shift()!);
        return;
      }

      const waiter = (evt: WsEvent | null) => {
        cleanup();
        resolve(evt);
      };
      if (!this.eventWaiters.has(sessionId)) {
        this.eventWaiters.set(sessionId, []);
      }
      this.eventWaiters.get(sessionId)!.push(waiter);

      const onAbort = () => {
        cleanup();
        resolve(null);
      };

      const cleanup = () => {
        if (abortSignal) {
          abortSignal.removeEventListener("abort", onAbort);
        }
        const waiters = this.eventWaiters.get(sessionId);
        if (waiters) {
          const idx = waiters.indexOf(waiter);
          if (idx >= 0) waiters.splice(idx, 1);
        }
      };

      if (abortSignal?.aborted) {
        onAbort();
        return;
      }
      if (abortSignal) {
        abortSignal.addEventListener("abort", onAbort);
      }
    });
  }

  /**
   * Continue processing events for a session that was running when the user
   * switched away. Does NOT yield to assistant-ui. Updates messagesMap
   * directly so the accumulated response is available when switching back.
   * Does NOT send chat.abort.
   */
  async processSessionInBackground(sessionId: string): Promise<void> {
    // Seed messagesMap with current state if empty
    let sessionMessages = this.messagesMap.get(sessionId);
    if (!sessionMessages) {
      sessionMessages = sessionId === this.sessionId ? [...this.messages] : [];
      this.messagesMap.set(sessionId, sessionMessages);
    }

    // Find existing partial AI message (from aborted run()), or create new
    const partialAi = sessionMessages.find((m) => m.role === "assistant" && m.liveStatus);
    const aiMsg: ChatMessage = partialAi ?? {
      id: `a_bg_${Date.now()}`,
      role: "assistant",
      content: "",
      parts: [],
      liveStatus: { status: "thinking" },
    };
    let currentText = partialAi?.content || "";
    let currentReasoning = "";
    const toolCalls = new Map<string, ChatMessagePart>();
    const extraParts: ChatMessagePart[] = [];
    if (!partialAi) {
      sessionMessages = [...sessionMessages, aiMsg];
      this.messagesMap.set(sessionId, sessionMessages);
    }

    try {
      while (true) {
        const evt = await this.nextEvent(sessionId);
        if (!evt) break;

        switch (evt.event) {
          case "chat.delta": {
            const delta = (evt.payload?.delta as string) || (evt.payload?.content as string) || "";
            currentText += delta;
            break;
          }
          case "agent.thinking": {
            currentReasoning += (evt.payload?.content as string) || "";
            break;
          }
          case "tool.calling": {
            const toolName = (evt.payload?.tool_name as string) || "tool";
            const rawArgs = evt.payload?.arguments;
            let toolArgs: Record<string, unknown> = {};
            if (typeof rawArgs === "string") {
              try { toolArgs = JSON.parse(rawArgs); } catch { toolArgs = { raw: rawArgs }; }
            } else if (rawArgs && typeof rawArgs === "object") {
              toolArgs = rawArgs as Record<string, unknown>;
            }
            toolCalls.set(`tc_bg_${toolCalls.size}_${Date.now()}`, {
              type: "tool-call",
              toolName,
              args: toolArgs as any,
            });
            break;
          }
          case "tool.result": {
            const tName = (evt.payload?.tool_name as string) || "";
            const result = evt.payload?.result;
            const data = evt.payload?.data;
            for (const [, tc] of toolCalls) {
              if (tc.toolName === tName && tc.result === undefined) {
                tc.result = result;
                tc.data = data;
                break;
              }
            }
            if ((tName === "write_report" || tName === "write_document") && data && typeof data === "object") {
              const d = data as Record<string, unknown>;
              extraParts.push({
                type: "document-ref",
                data: {
                  filename: d.filename,
                  title: d.title || d.filename,
                  format: d.format || "markdown",
                  url: d.url,
                  export_url: d.export_url,
                },
              });
            }
            break;
          }
          case "chat.final": {
            currentText = (evt.payload?.response as string) || currentText;
            const finalParts: ChatMessagePart[] = [];
            if (currentReasoning) finalParts.push({ type: "reasoning", text: currentReasoning });
            for (const tc of toolCalls.values()) finalParts.push(tc);
            finalParts.push(...extraParts);
            if (currentText) finalParts.push({ type: "text", text: currentText });

            aiMsg.content = currentText;
            aiMsg.parts = finalParts;
            aiMsg.durationMs = Date.now();
            aiMsg.toolCount = toolCalls.size;
            aiMsg.turnId = (evt.payload?.turn_id as string) || undefined;
            aiMsg.liveStatus = undefined;

            this.messagesMap.set(sessionId, [...sessionMessages]);
            this.saveHistory(sessionId, this.messagesMap.get(sessionId)!);

            // If user switched back and this is now the current session, notify listeners
            if (sessionId === this.sessionId) {
              this.messages = this.messagesMap.get(sessionId)!;
              this.messagesListeners.forEach((cb) => cb(this.messages));
            }
            return;
          }
          case "chat.error": {
            aiMsg.content = currentText || ((evt.payload?.message as string) || "Chat error");
            aiMsg.liveStatus = undefined;
            this.messagesMap.set(sessionId, [...sessionMessages]);
            return;
          }
        }

        // Update partial state in messagesMap
        const parts: ChatMessagePart[] = [];
        if (currentReasoning) parts.push({ type: "reasoning", text: currentReasoning });
        for (const tc of toolCalls.values()) parts.push(tc);
        parts.push(...extraParts);
        if (currentText) parts.push({ type: "text", text: currentText });

        aiMsg.content = currentText;
        aiMsg.parts = parts;
        this.messagesMap.set(sessionId, sessionMessages);

        // If this is now the current session, notify live
        if (sessionId === this.sessionId) {
          this.messages = sessionMessages;
          this.messagesListeners.forEach((cb) => cb(this.messages));
        }
      }
    } catch {
      // Background processor silently handles errors
    } finally {
      this.runningSessions.delete(sessionId);
      this.notifyRunningSessionsChanged();
    }
  }
}


function makeTextPart(text: string): TextMessagePart {
  return { type: "text", text };
}

function makeReasoningPart(text: string): ReasoningMessagePart {
  return { type: "reasoning", text };
}

function makeToolCallPart(
  toolCallId: string,
  toolName: string,
  args: Record<string, unknown>,
  result?: unknown,
  data?: unknown
): ToolCallMessagePart {
  return {
    type: "tool-call",
    toolCallId,
    toolName,
    args,
    argsText: JSON.stringify(args),
    result,
    data,
  } as ToolCallMessagePart;
}

/** Convert assistant-ui part to ChatMessagePart preserving metadata. */
function toChatPart(
  p: TextMessagePart | ReasoningMessagePart | ToolCallMessagePart
): ChatMessagePart {
  const data = (p as any).data;
  if (p.type === "tool-call") {
    return {
      type: p.type,
      toolName: p.toolName,
      args: p.args,
      result: p.result,
      data,
    };
  }
  const result: ChatMessagePart = { type: p.type, text: (p as any).text || "" };
  if (data !== undefined) {
    result.data = data;
  }
  return result;
}

/**
 * Syscity WebSocket-native protocol client implementing ChatModelAdapter.
 *
 * Uses AsyncGenerator to stream updates for assistant-ui consumption.
 */

export interface SyscityWebSocketTransport {
  listAgents(): Promise<{ agents: string[] }>;
  getAgent(agentId: string): Promise<{ agent_id: string; busy: boolean; status: string; config: Record<string, unknown> | null; personality: Record<string, unknown> | null; } | null>;
  addChannel(payload: { name: string; channel_type: string; enabled?: boolean; agent_id?: string; credentials?: Record<string, string> }): Promise<boolean>;
  updateChannel(payload: { name: string; enabled?: boolean; agent_id?: string; credentials?: Record<string, string> }): Promise<boolean>;
  removeChannel(name: string): Promise<boolean>;
  setChannelEnabled(name: string, enabled: boolean): Promise<boolean>;
  getCloudStatus(): Promise<unknown>;
  getCloudSubscription(): Promise<unknown>;
  getCloudUsage(days?: number): Promise<unknown>;
  submitCloudToken(token: string): Promise<unknown>;
  cloudLogout(): Promise<void>;
  cloudKbList(): Promise<unknown>;
  cloudKbCreate(name: string): Promise<unknown>;
  cloudKbDelete(kbId: string): Promise<{ ok: boolean }>;
  cloudKbDocs(kbId: string): Promise<unknown>;
  /** Back up one local collection (`kb-{agent_id}`) to the cloud. Long-running
   * (one upload per changed document) — 5 min WS timeout. */
  cloudKbPush(
    collection: string
  ): Promise<{
    collection: string;
    cloud_kb_id: string;
    cloud_kb_name: string;
    total: number;
    pushed: number;
    unchanged: number;
    skipped_url: number;
    skipped_external: number;
    too_large: number;
    failed: number;
    errors: string[];
  }>;
  /** Restore a cloud backup into a local agent collection. Long-running —
   * 5 min WS timeout. */
  cloudKbPull(
    params: { collection: string } | { cloud_kb_id: string; agent_id: string }
  ): Promise<{
    collection: string;
    agent_id: string;
    cloud_kb_id: string;
    total: number;
    pulled: number;
    unchanged: number;
    failed: number;
    errors: string[];
  }>;
  getConfig(): Promise<Record<string, unknown>>;
  setConfig(path: string, value: unknown): Promise<boolean>;
  listCrons(): Promise<{ jobs: Array<Record<string, unknown>>; count: number }>;
  requestMacosAccessibility(): Promise<{ status: string; message: string } | null>;
  deviceCapabilities(): Promise< Array<{ id: string; label: string; available: boolean; granted: boolean }> | null >;
  devicePermissionStatus(permission: string): Promise<{ granted: boolean; state: string } | null>;
  requestDevicePermission(permission: string): Promise<{ granted: boolean; state: string } | null>;
  adbStatus(): Promise<{ paired: boolean; devices: Array<{ serial: string; state: string }> } | null>;
  adbPair( port: number, code: string, connectPort?: number ): Promise<{ paired: boolean; connected: boolean; pairOutput?: string; connectOutput?: string; devices: Array<{ serial: string; state: string }>; } | null>;
  getEvalDashboard(): Promise<EvalDashboardPayload>;
  emptyEvalDashboard(): EvalDashboardPayload;
  getFeedbackOps(): Promise<FeedbackOpsPayload>;
  emptyFeedbackOps(): FeedbackOpsPayload;
  getConnectorsCatalog(lang?: string): Promise<unknown>;
  listMcpPresets(): Promise< Array<{ name: string; display_name: string; description: string; logo_url?: string; command?: string; args: string[]; url?: string; transport: string; enabled: boolean; auth_type?: string; client_id?: string; auth_url?: string; token_url?: string; scopes?: string; env: Array<{ name: string; required: boolean; description?: string }>; }> >;
  listMcpServers(): Promise<{ servers: Array<{ id: string; transport: string; command?: string; args: string[]; url?: string; auto_connect: boolean; connected: boolean; env_configured?: boolean; }>; }>;
  addMcpServer(payload: { id: string; transport: string; command?: string; args?: string[]; url?: string; auth_type?: string; client_id?: string; auth_url?: string; token_url?: string; scopes?: string; auto_connect?: boolean; env?: Record<string, string>; }): Promise<{ ok: boolean; error?: string }>;
  removeMcpServer(id: string): Promise<boolean>;
  connectMcpServer(id: string): Promise<{ ok: boolean; error?: string; errorCode?: string; authUrl?: string }>;
  disconnectMcpServer(id: string): Promise<boolean>;
  cancelMcpAuth(serverId: string): Promise<boolean>;
  listModels(): Promise<{ models: ModelInfo[]; default_model: string }>;
  listModelPresets(): Promise<Array<{ name: string; display_name: string; base_url?: string; models: string[]; protocol?: "open_ai" | "anthropic" | "gemini"; needs_api_key?: boolean }>>;
  fetchRemoteModels(payload: { provider: string; base_url?: string; api_key?: string; protocol?: "open_ai" | "anthropic" | "gemini" }): Promise<{ models: string[]; source: "remote" | "static"; error?: string }>;
  addModel(payload: { provider: string; models: string[]; default_model?: string; api_key?: string; base_url?: string }): Promise<{ ok: boolean; error?: string }>;
  removeModel(modelId: string): Promise<boolean>;
  setDefaultModel(modelId: string): Promise<boolean>;
  setSessionModel( sessionId: string, model: string | null ): Promise<boolean>;
  setSessionPinned( sessionId: string, pinned: boolean ): Promise<boolean>;
  respondToApproval(approvalId: string, decision: "approve" | "deny", reason?: string): Promise<boolean>;
  renameSession(sessionId: string, name: string): Promise<boolean>;
  respondToAsk(askId: string, response: string): Promise<boolean>;
  vote( turnId: string, vote: "up" | "down", opts?: { input?: string; response?: string; comment?: string } ): Promise<boolean>;
  runShortcut(name: string, input?: string): Promise<{ launched: boolean } | null>;
  shortcutResults(): Promise<Array<{ output?: string; at_ms?: number; file?: string }> | null>;
  shortcutInbox(): Promise<Array<{ prompt?: string; at_ms?: number; file?: string }> | null>;
  listSkills(): Promise<{ skills: Array<Record<string, unknown>>; count: number }>;
  installSkill(name: string, zipBase64: string): Promise<boolean>;
  listKbCollections(): Promise<{ configured: boolean; reason: string | null; collections: Array<Record<string, unknown>> }>;
  listKbDocs(collection: string): Promise<{ collection: string; docs: Array<Record<string, unknown>> }>;
  kbDocContent(collection: string, docId: string): Promise<{ doc_id: string; size: number; truncated: boolean; binary: boolean; content?: string }>;
  ingestKbDoc(agentId: string, filename: string, contentBase64: string): Promise<Record<string, unknown>>;
  deleteKbDoc(collection: string, docId: string): Promise<{ collection: string; doc_id: string; chunks_deleted: number }>;
  getUpdateStatus(): Promise<unknown>;
  getUpdateProgress(): Promise<unknown>;
  triggerUpdate(): Promise<unknown>;
  workspaceList( agentId: string | undefined, path: string ): Promise<{ root: string; path: string; entries: Array<{ name: string; path: string; kind: "dir" | "file"; size: number; modified?: number; }>; }>;
  workspaceRead( agentId: string | undefined, path: string ): Promise<{ path: string; size: number; truncated: boolean; binary: boolean; content?: string; }>;
}
