import { useMemo, useState, useEffect, useCallback, useRef } from "react";
import {
  AssistantRuntimeProvider,
  useLocalRuntime,
} from "@assistant-ui/react";
import {
  SyscityWebSocketTransport,
  setActiveTransport,
  type ChatMessage,
} from "@/SyscityWebSocketTransport";
import { cloudSubmitToken } from "@/lib/cloud";
import { useChatStore } from "@/stores/chatStore";
import { Titlebar } from "@/components/chrome/Titlebar";
import { Statusbar } from "@/components/chrome/Statusbar";
import { Sidebar } from "@/components/chat/Sidebar";
import { SettingsPanel } from "@/components/settings/SettingsPanel";
import { WelcomeScreen } from "@/components/onboarding/WelcomeScreen";
import { IdentityWizard } from "@/components/onboarding/IdentityWizard";
import { ChatContent } from "@/components/chat/ChatContent";
import { useGoalStore } from "@/stores/goalStore";
import { GoalPanel } from "@/components/chat/GoalPanel";
import { DocumentPreviewPanel } from "@/components/shared/DocumentPreviewPanel";
import { WorkspacePanel } from "@/components/workspace/WorkspacePanel";
import { UpdateBanner } from "@/components/update/UpdateBanner";
import { CloudEnabledBanner } from "@/components/update/CloudEnabledBanner";
import { ExtensionsView } from "@/components/marketplace/ExtensionsView";
import { KnowledgeBaseView } from "@/components/kb/KnowledgeBaseView";
import { AskModal, type AskPrompt } from "@/components/ask/AskModal";
import { ApprovalModal } from "@/components/approval/ApprovalModal";
import type { ApprovalPrompt } from "@/components/approval/ApprovalModal";
import { useIsMobile } from "@/hooks/useMediaQuery";

/* ── Agent emoji pool — ensures each agent has a unique icon ── */
const EMOJI_POOL = [
  "🦑", "🐙", "🦊", "🐺", "🐱", "🐼", "🐨", "🦁",
  "🐯", "🐸", "🦄", "🐲", "🦅", "🦉", "🐳", "🦋",
  "🐞", "🦀", "🕷️", "🐝", "🐢", "🦎", "🦈", "🐧",
];

function assignUniqueEmojis(
  agents: Array<{ id: string; emoji: string }>
): Map<string, string> {
  const result = new Map<string, string>();
  const used = new Set<string>();
  let poolIdx = 0;

  for (const a of agents) {
    let emoji = a.emoji;
    // Use agent's own emoji if it's non-empty and not already taken
    if (emoji && !used.has(emoji)) {
      used.add(emoji);
    } else {
      // Assign next unused emoji from pool
      while (poolIdx < EMOJI_POOL.length && used.has(EMOJI_POOL[poolIdx])) {
        poolIdx++;
      }
      emoji = poolIdx < EMOJI_POOL.length ? EMOJI_POOL[poolIdx++] : "🤖";
      used.add(emoji);
    }
    result.set(a.id, emoji);
  }
  return result;
}

/* ── ChatAppInner ── */
function ChatAppInner({ transport }: { transport: SyscityWebSocketTransport }) {
  const runtime = useLocalRuntime(transport);

  useEffect(() => {
    let cancelled = false;
    const doLoad = async () => {
      // Welcome page armed (agent summon): don't pull the previous session's
      // history into it — armNewSession leaves sessionId pointing at the old
      // session, so a remount-triggered load would clobber the empty view.
      if (transport.isPendingNewSession()) return;
      try {
        const { messages: history, hasMore } = await transport.loadHistory(
          transport.getSessionId()
        );
        if (cancelled) return;
        transport.setMessages(history);
        useChatStore.getState().setMessages(history);
        useChatStore.getState().setHasMoreHistory(hasMore);
      } catch {
        /* ignore — will retry when connected */
      }
    };
    doLoad();

    // Retry loading history when connection is established
    const unsubStatus = transport.onStatusChange((status) => {
      if (
        status === "connected" &&
        transport.getMessages().length === 0 &&
        // Welcome page armed: don't pull the previous session's history into it
        !transport.isPendingNewSession()
      ) {
        doLoad();
      }
    });

    // Subscribe to message changes
    const unsub = transport.onMessagesChange((msgs) => {
      const prev = useChatStore.getState().messages;
      useChatStore.getState().setMessages([...msgs]);
      // Freshly armed welcome page ("/new" included): messages just
      // transitioned to empty — clear a pending agent summon. The immediate
      // replay on subscribe (e.g. ChatAppInner remounting right after an
      // agent summon) re-delivers an already-empty list and must not wipe
      // the summon that was just set.
      if (msgs.length === 0 && prev.length > 0) {
        useChatStore.getState().setPendingAgent(null);
      }
    });
    return () => {
      cancelled = true;
      unsub();
      unsubStatus();
    };
  }, [transport]);

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <ChatContent transport={transport} />
    </AssistantRuntimeProvider>
  );
}

/* ── ChatApp ── */
function ChatApp() {
  const transport = useMemo(() => new SyscityWebSocketTransport(), []);
  useEffect(() => {
    setActiveTransport(transport);
  }, [transport]);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() => {
    return localStorage.getItem("syscity_sidebar_collapsed") === "true";
  });
  const [sessions, setSessions] = useState<
    Array<{
      id: string;
      label?: string;
      agent_id?: string;
      pinned?: boolean;
      model?: string | null;
      last_activity?: number;
    }>
  >([]);
  const [agents, setAgents] = useState<
    Array<{
      id: string;
      display_name: string;
      emoji: string;
      is_valid: boolean;
      has_heartbeat: boolean;
    }>
  >([]);
  const [runningSessionIds, setRunningSessionIds] = useState<string[]>([]);
  const [sessionKey, setSessionKey] = useState(0);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // Tab to open settings on (e.g. "marketplace" via the sidebar shortcut).
  const [settingsTab, setSettingsTab] = useState("general");
  // Full-screen marketplace view (replaces the chat area).
  const [marketplaceOpen, setMarketplaceOpen] = useState(false);
  // Pre-filter for the marketplace view (connector/skill/expert; null = all).
  const [marketplaceType, setMarketplaceType] = useState<string | null>(null);
  // Full-screen Knowledge Base management view (local + cloud KBs).
  const [kbOpen, setKbOpen] = useState(false);
  // null = not yet checked / not connected; true = no LLM configured (Welcome).
  const [needsSetup, setNeedsSetup] = useState<boolean | null>(null);
  // null = not yet checked; true = identity wizard completed.
  const [onboardingDone, setOnboardingDone] = useState<boolean | null>(null);

  /** Open settings on a specific tab (e.g. "marketplace" from the sidebar). */
  const openSettings = (tab: string) => {
    setMarketplaceOpen(false);
    setSettingsTab(tab);
    setSettingsOpen(true);
  };

  /** Open the full-screen marketplace view (replaces the chat area),
   * optionally pre-filtered to one catalog type (connector/skill/expert). */
  const openMarketplace = (type?: string) => {
    setMarketplaceType(type ?? null);
    setSettingsOpen(false);
    setKbOpen(false);
    setMarketplaceOpen(true);
  };

  /** Open the full-screen Knowledge Base view (replaces the chat area). */
  const openKb = () => {
    setSettingsOpen(false);
    setMarketplaceOpen(false);
    setKbOpen(true);
  };

  // Handle the cloud OAuth callback: /cloud/login/callback#token=... — persist
  // the session token, then return to the home view.
  useEffect(() => {
    if (window.location.pathname === "/cloud/login/callback") {
      const token = new URLSearchParams(window.location.hash.slice(1)).get(
        "token",
      );
      // In the sidebar popup flow this page runs inside the OAuth popup
      // (window.opener present): persist the token, notify the opener, then
      // self-close. Otherwise (direct navigation, e.g. from the welcome page)
      // fall back to returning to the home view.
      const isPopup = Boolean(window.opener);
      const finish = () => {
        if (isPopup) {
          try {
            window.close();
          } catch {
            /* ignore */
          }
        } else {
          window.location.replace("/");
        }
      };
      if (token) {
        cloudSubmitToken(token)
          .then((ok) => {
            // First-login guidance: surface the newly-enabled cloud
            // capabilities banner on the home view after a successful login.
            if (ok) {
              try {
                localStorage.setItem("syscity_cloud_enabled_hint", "1");
              } catch {
                /* ignore */
              }
            }
            if (isPopup) {
              // Tell the opener the flow finished (it polls /api/v1/status as
              // the reliable fallback). targetOrigin "*": the opener may be on
              // 127.0.0.1 while this callback is on localhost — the receiver
              // validates the origin.
              try {
                window.opener.postMessage(
                  { type: "syscity:login", ok },
                  "*",
                );
              } catch {
                /* ignore */
              }
            }
          })
          .finally(finish);
      } else {
        finish();
      }
    }
  }, []);

  const isMobile = useIsMobile();
  const [mobileNavOpen, setMobileNavOpen] = useState(false);
  const previewDocument = useChatStore((s) => s.previewDocument);
  const setPreviewDocument = useChatStore((s) => s.setPreviewDocument);
  // A pending ask_user question awaiting a human answer.
  const [askPrompt, setAskPrompt] = useState<AskPrompt | null>(null);
  const [pendingApprovals, setPendingApprovals] = useState<ApprovalPrompt[]>([]);
  const approvalPrompt = pendingApprovals[0] ?? null;
  const workspacePanelOpen = useChatStore((s) => s.workspacePanelOpen);
  const setWorkspacePanelOpen = useChatStore((s) => s.setWorkspacePanelOpen);
  const currentAgent = useChatStore((s) => s.currentAgent);
  // Message count drives the "welcome page" state (empty chat = New Session
  // welcome), which the sidebar uses to highlight the right nav item.
  const messageCount = useChatStore((s) => s.messages.length);

  // Resizable split panel state
  const [previewRatio, setPreviewRatio] = useState(() => {
    const saved = localStorage.getItem("syscity_preview_ratio");
    return saved ? Math.max(0.2, Math.min(0.8, parseFloat(saved))) : 0.45;
  });
  const [isDragging, setIsDragging] = useState(false);
  const splitContainerRef = useRef<HTMLDivElement>(null);
  const previewRatioRef = useRef(previewRatio);

  // Keep ref in sync
  useEffect(() => {
    previewRatioRef.current = previewRatio;
    localStorage.setItem("syscity_preview_ratio", String(previewRatio));
  }, [previewRatio]);

  // Sidebar state persistence
  useEffect(() => {
    localStorage.setItem("syscity_sidebar_collapsed", String(sidebarCollapsed));
  }, [sidebarCollapsed]);

  // Load sessions
  const refreshSessions = useCallback(async () => {
    const list = await transport.listSessions();
    setSessions(list);
  }, [transport]);

  // Load agents
  const refreshAgents = useCallback(async () => {
    const list = await transport.listAgentRegistry();
    // Exclude the default agent and any invalid entries.
    const filtered = list.filter(
      (a) => a.is_valid && a.id !== "default"
    );
    // Assign unique emojis so each agent has a distinct icon.
    const emojiMap = assignUniqueEmojis(filtered);
    setAgents(
      filtered.map((a) => ({ ...a, emoji: emojiMap.get(a.id) || "🤖" }))
    );
  }, [transport]);

  // Check whether an LLM is configured. When no models exist, show the
  // first-launch Welcome screen so the user can add their initial model.
  const checkModelConfig = useCallback(async () => {
    try {
      const { models } = await transport.listModels();
      setNeedsSetup(models.length === 0);
    } catch {
      /* keep needsSetup as-is; retried on next connect */
    }
  }, [transport]);

  // Check whether the first-launch identity wizard is still pending.
  const checkOnboarding = useCallback(async () => {
    try {
      const { status } = await transport.onboardingStatus();
      setOnboardingDone(status === "done");
    } catch {
      /* keep onboardingDone as-is; retried on next connect */
    }
  }, [transport]);

  // Network status — lives in chatStore so Titlebar/Statusbar read it
  // without prop drilling.
  useEffect(() => {
    return transport.onStatusChange((status) => {
      useChatStore.getState().setNetworkStatus(status);
      if (status === "connected") {
        refreshSessions();
        refreshAgents();
        checkModelConfig();
        checkOnboarding();
      }
    });
  }, [transport, refreshSessions, refreshAgents, checkModelConfig, checkOnboarding]);

  useEffect(() => {
    refreshSessions();
    refreshAgents();
    checkModelConfig();
    checkOnboarding();
    const interval = setInterval(() => {
      refreshSessions();
      refreshAgents();
    }, 8000);
    return () => clearInterval(interval);
  }, [refreshSessions, refreshAgents, checkModelConfig, checkOnboarding]);

  // Once an LLM is configured, resolve the identity wizard state so we don't
  // linger on the loading gate if the mount/connect checks raced model setup.
  useEffect(() => {
    if (needsSetup === false && onboardingDone === null) {
      checkOnboarding();
    }
  }, [needsSetup, onboardingDone, checkOnboarding]);

  // Track which sessions are currently running for sidebar loading indicators
  useEffect(() => {
    return transport.onRunningSessionsChange((ids) => {
      setRunningSessionIds(ids);
    });
  }, [transport]);

  // Refresh session list immediately when transport creates/switches sessions
  useEffect(() => {
    return transport.onSessionChange(() => {
      refreshSessions();
    });
  }, [transport, refreshSessions]);

  // Listen for new sessions, renames, and cron results
  useEffect(() => {
    return transport.onEvent((evt) => {
      if (evt.event === "ask.required") {
        const p = evt.payload;
        if (!p) return;
        setAskPrompt({
          ask_id: String(p.ask_id ?? ""),
          session_id: String(p.session_id ?? ""),
          question: String(p.question ?? ""),
          options: Array.isArray(p.options) ? p.options.map(String) : [],
          required: (p.required as boolean) ?? true,
          default: typeof p.default === "string" ? p.default : undefined,
        });
      }
      if (evt.event === "ask.resolved") {
        const p = evt.payload;
        if (!p) return;
        setAskPrompt((prev) =>
          prev && prev.ask_id === p.ask_id ? null : prev
        );
      }
      if (evt.event === "approval.required") {
        const p = evt.payload;
        if (!p) return;
        setPendingApprovals((prev) => [
          ...prev,
          {
            approval_id: String(p.approval_id ?? ""),
            tool_name: String(p.tool_name ?? ""),
            requested_by: String(p.requested_by ?? ""),
            risk_level: String(p.risk_level ?? "Medium"),
            message: String(p.message ?? ""),
          },
        ]);
      }
      if (evt.event === "session.created") {
        refreshSessions();
      }
      if (evt.event === "session.renamed") {
        const p = evt.payload as Record<string, string> | undefined;
        if (!p) return;
        const renamedSessionId = p.session_id;
        const newName = p.name;
        setSessions((prev) =>
          prev.map((s) =>
            s.id === renamedSessionId
              ? { ...s, label: newName }
              : s
          )
        );
      }
      if (evt.event === "session.pinned") {
        const p = evt.payload as Record<string, unknown> | undefined;
        if (!p) return;
        const pinnedSessionId = p.session_id as string;
        const pinned = !!p.pinned;
        setSessions((prev) =>
          prev.map((s) =>
            s.id === pinnedSessionId ? { ...s, pinned } : s
          )
        );
      }
      if (evt.event === "session.model_changed") {
        const p = evt.payload as Record<string, unknown> | undefined;
        if (!p) return;
        const modelSessionId = p.session_id as string;
        const model = (p.model as string | null) ?? null;
        setSessions((prev) =>
          prev.map((s) =>
            s.id === modelSessionId ? { ...s, model } : s
          )
        );
      }
      if (evt.event === "cron.completed") {
        const p = evt.payload as Record<string, string> | undefined;
        if (!p) return;
        const jobName = p.job_name || "cron job";
        const status = p.status || "ok";
        const output = p.output || "";
        const runAt = p.run_at || "";
        const icon = status === "ok" ? "✅" : "❌";
        const text = `${icon} **${jobName}**\n\n${output}\n\n_Executed at ${runAt}_`;
        const msg: ChatMessage = {
          id: `cron_${Date.now()}`,
          role: "assistant",
          content: text,
          parts: [{ type: "text", text }],
          timestamp: Date.now(),
        };
        transport.saveMessage(msg);
        const updated = [...transport.getMessages(), msg];
        transport.setMessages(updated);
      }
      if (evt.event === "goal.progress") {
        const p = evt.payload as Record<string, unknown> | undefined;
        if (!p) return;
        const goalId = (p.goal_id as string) || "";
        const goalEvent = p.event as Record<string, unknown> | undefined;
        if (!goalEvent) return;
        const subEvent = (goalEvent.event as string) || "";

        // Update goal store for the dedicated GoalPanel.
        const updateGoal = useGoalStore.getState().updateGoal;
        if (subEvent === "goal.started") {
          const desc = (goalEvent.description as string) || "";
          const conditions = (goalEvent.conditions as string[]) || [];
          const maxRounds = (goalEvent.max_rounds as number) || 5;
          useGoalStore.setState((s) => ({
            goals: {
              ...s.goals,
              [goalId]: {
                id: goalId,
                description: desc,
                conditions,
                maxRounds,
                round: 0,
                passed: 0,
                total: conditions.length,
                status: "running",
              },
            },
          }));
        } else if (subEvent === "goal.check") {
          updateGoal(goalId, {
            round: (goalEvent.round as number) || 0,
            passed: (goalEvent.passed as number) || 0,
            total: (goalEvent.total as number) || 0,
          });
        } else if (subEvent === "goal.done") {
          updateGoal(goalId, {
            status: "done",
            summary: (goalEvent.summary as string) || "",
          });
        } else if (subEvent === "goal.aborted") {
          updateGoal(goalId, {
            status: "aborted",
            reason: (goalEvent.reason as string) || "",
            round: (goalEvent.round as number) || 0,
          });
        }

        // Build chat message (existing behavior).
        let text = "";
        if (subEvent === "goal.started") {
          const desc = (goalEvent.description as string) || "";
          const conditions = (goalEvent.conditions as string[]) || [];
          const maxRounds = (goalEvent.max_rounds as number) || 5;
          const condList = conditions.map((c: string) => `  - ${c}`).join("\n");
          text = `🎯 **Goal Started**: ${desc}\n\n**Conditions** (must all pass):\n${condList}\n\n_Max rounds: ${maxRounds}_`;
        } else if (subEvent === "goal.check") {
          const round = (goalEvent.round as number) || 0;
          const passed = (goalEvent.passed as number) || 0;
          const total = (goalEvent.total as number) || 0;
          text = `🔍 **Goal Check — Round ${round}**: ${passed}/${total} conditions passed`;
        } else if (subEvent === "goal.retry") {
          const round = (goalEvent.round as number) || 0;
          const feedback = (goalEvent.feedback as string) || "";
          text = `🔄 **Goal Retry — Round ${round}**\n\n${feedback}`;
        } else if (subEvent === "goal.done") {
          const summary = (goalEvent.summary as string) || "";
          text = `✅ **Goal Complete**\n\n${summary}`;
        } else if (subEvent === "goal.aborted") {
          const reason = (goalEvent.reason as string) || "";
          text = `⛔ **Goal Aborted**: ${reason}`;
        }
        if (text) {
          const msg: ChatMessage = {
            id: `goal_${Date.now()}`,
            role: "assistant",
            content: text,
            parts: [{ type: "text", text }],
            timestamp: Date.now(),
          };
          transport.saveMessage(msg);
          const updated = [...transport.getMessages(), msg];
          transport.setMessages(updated);
        }
      }
    });
  }, [transport, refreshSessions]);

  const handleAskRespond = useCallback(
    async (response: string) => {
      if (!askPrompt) return;
      await transport.respondToAsk(askPrompt.ask_id, response);
      setAskPrompt(null);
    },
    [askPrompt, transport]
  );

  const handleAskDismiss = useCallback(() => {
    // No server resolve: the blocked tool times out server-side (5 min) and
    // broadcasts ask.resolved(cancelled), which clears this modal.
    setAskPrompt(null);
  }, []);

  const handleApprovalDecide = useCallback(
    async (decision: "approve" | "deny", reason?: string) => {
      if (!approvalPrompt) return;
      await transport.respondToApproval(approvalPrompt.approval_id, decision, reason);
      setPendingApprovals((prev) => prev.slice(1));
    },
    [approvalPrompt, transport]
  );

  const handleApprovalDismiss = useCallback(() => {
    // The approval stays pending server-side; the next turn that needs it
    // will re-emit approval.required, or the user acts via the badge.
    setPendingApprovals((prev) => prev.slice(1));
  }, []);

  const handleNewSession = useCallback(() => {
    setSettingsOpen(false);
    setMarketplaceOpen(false);
    setKbOpen(false);
    // New Session is a page toggle: show the welcome state without creating
    // anything. The real session is created lazily on the first message sent
    // from the welcome page (transport.run() consumes the pending flag).
    useChatStore.getState().setPendingAgent(null);
    transport.armNewSession();
  }, [transport]);

  const handleCreateSessionWithAgent = useCallback(
    async (agentId: string) => {
      setSettingsOpen(false);
      setMarketplaceOpen(false);
      setKbOpen(false);

      // If a session already exists for this agent, switch to it instead
      // of creating another one.
      const existing = sessions.find((s) => s.agent_id === agentId);
      if (existing) {
        useChatStore.getState().setPendingAgent(null);
        transport.switchSession(existing.id);
        const { messages: history, hasMore } = await transport.loadHistory(existing.id);
        transport.setMessages(history);
        useChatStore.getState().setHasMoreHistory(hasMore);
        setSessionKey((k) => k + 1);
        await refreshSessions();
        return;
      }

      // No session yet: navigate to the New Session welcome page with the
      // agent attached. The session is created lazily on the first message
      // (same deferred flow as the New Session button) — clicking an agent
      // and never typing no longer leaves an empty session behind.
      transport.armNewSession(agentId);
      const agent = agents.find((a) => a.id === agentId);
      // Set after armNewSession: its empty-messages listener clears any
      // previous pending agent.
      useChatStore.getState().setPendingAgent(
        agent
          ? { id: agent.id, display_name: agent.display_name, emoji: agent.emoji }
          : { id: agentId, display_name: agentId, emoji: "🤖" }
      );
      setSessionKey((k) => k + 1);
    },
    [transport, refreshSessions, sessions, agents]
  );

  const handleSwitchSession = useCallback(
    async (id: string) => {
      setSettingsOpen(false);
      setMarketplaceOpen(false);
      setKbOpen(false);
      // Navigating away from the welcome page consumes any pending summon.
      useChatStore.getState().setPendingAgent(null);
      const currentId = transport.getSessionId();
      if (currentId !== id) {
        // Save current session's in-memory messages before switching
        const currentMessages = transport.getMessages();
        if (currentMessages.length > 0) {
          transport.saveHistory(currentId, currentMessages);
        }
        // Store current messages in messagesMap for background processing
        transport.saveSessionMessages(currentId, currentMessages);
        // Stop local processing but keep backend running
        transport.abortLocal(currentId);
        // Continue processing events in background
        transport.processSessionInBackground(currentId).catch(() => {});
      }
      // Switch to new session
      transport.switchSession(id);
      // Try cached messages first, fallback to backend history
      const cached = transport.getSessionMessages(id);
      if (cached.length > 0) {
        transport.setMessages(cached);
      } else {
        const { messages: history, hasMore } = await transport.loadHistory(id);
        transport.setMessages(history);
        useChatStore.getState().setHasMoreHistory(hasMore);
      }
      setSessionKey((k) => k + 1);
    },
    [transport]
  );

  const handleRenameSession = useCallback(
    async (id: string, name: string) => {
      await transport.renameSession(id, name);
      refreshSessions();
    },
    [transport, refreshSessions]
  );

  const handleDeleteSession = useCallback(
    async (id: string) => {
      await transport.deleteSession(id);
      refreshSessions();
      setSessionKey((k) => k + 1);
    },
    [transport, refreshSessions]
  );

  const handlePinSession = useCallback(
    async (id: string, pinned: boolean) => {
      await transport.setSessionPinned(id, pinned);
      refreshSessions();
    },
    [transport, refreshSessions]
  );

  // Build session items enriched with agent info for sidebar badges.
  const sessionItems = useMemo(() => {
    return sessions.map((s) => ({
      id: s.id,
      label: s.label,
      pinned: s.pinned,
      last_activity: s.last_activity,
      agent: agents.find((a) => a.id === s.agent_id),
    }));
  }, [sessions, agents]);

  // Keep chatStore.currentAgent in sync with the active session.
  useEffect(() => {
    const currentId = transport.getSessionId();
    const current = sessionItems.find((s) => s.id === currentId);
    useChatStore.getState().setCurrentAgent(current?.agent);
    // A pending summon lives only while the welcome page is armed; the
    // summon itself doesn't change sessionId, so a matching "current" here
    // is just the stale previous session — clear only once the flag has
    // been consumed (first send created the real session).
    if (!transport.isPendingNewSession()) {
      useChatStore.getState().setPendingAgent(null);
    }
  }, [sessionItems, transport]);

  // Gate: until we have confirmed whether an LLM is configured, show a
  // minimal centered loading screen (prevents flashing the chat).
  if (needsSetup === null) {
    return (
      <div
        className="flex items-center justify-center bg-page text-primary"
        style={{
          paddingTop: "env(safe-area-inset-top)",
          paddingBottom: "env(safe-area-inset-bottom)",
          height: "100lvh",
        }}
      >
        <div className="w-6 h-6 border-2 border-subtle border-t-primary-500 rounded-full animate-spin" />
      </div>
    );
  }

  // No LLM configured yet — show the first-launch welcome screen.
  if (needsSetup) {
    return <WelcomeScreen transport={transport} onComplete={checkModelConfig} />;
  }

  // LLM configured but the identity wizard state is still resolving.
  if (onboardingDone === null) {
    return (
      <div
        className="flex items-center justify-center bg-page text-primary"
        style={{
          paddingTop: "env(safe-area-inset-top)",
          paddingBottom: "env(safe-area-inset-bottom)",
          height: "100lvh",
        }}
      >
        <div className="w-6 h-6 border-2 border-subtle border-t-primary-500 rounded-full animate-spin" />
      </div>
    );
  }

  // LLM configured but identity not set yet — show the first-launch wizard.
  if (!onboardingDone) {
    return <IdentityWizard transport={transport} onComplete={checkOnboarding} />;
  }

  // iOS WKWebView computes the layout viewport as safe-area-exclusive
  // (~759pt on iPhone 16) at rest, so 100%/100dvh leave a gap below the
  // composer, while 100lvh resolves to the full screen (852pt) consistently.
  // Do NOT drive the root height from visualViewport: vv.height drifts
  // 759<->852 without emitting resize, which re-introduces the gap.

  return (
    <div
      className="flex flex-col bg-page text-primary"
      style={{
        // iOS WKWebView computes the layout viewport as safe-area-exclusive
        // (~759pt on iPhone 16) at rest, so 100%/100dvh leave a gap below the
        // composer. 100lvh resolves to the full screen (852pt), and shrinks
        // with the keyboard via interactive-widget=resizes-content.
        height: "100lvh",
      }}
    >
      {/* Row 1: app titlebar (macOS overlay: traffic lights + drag region). */}
      <Titlebar
        isMobile={isMobile}
        showHamburger={isMobile && !mobileNavOpen}
        sidebarCollapsed={sidebarCollapsed}
        onToggleSidebar={() => setSidebarCollapsed((c) => !c)}
        onOpenMobileNav={() => setMobileNavOpen(true)}
        onOpenSettings={() => openSettings("general")}
        page={
          kbOpen
            ? { title: "Knowledge Base" }
            : marketplaceOpen
              ? { title: "Extensions" }
              : undefined
        }
      />

      {/* Row 2: sidebar + main content. */}
      <div className="flex flex-1 min-h-0 overflow-hidden">
        {/* Desktop: inline sidebar. Mobile: hidden, the drawer below replaces it. */}
        <div className="hidden md:contents">
          <Sidebar
            collapsed={sidebarCollapsed}
            onToggle={() => setSidebarCollapsed((c) => !c)}
            sessions={sessionItems}
            currentSessionId={transport.getSessionId()}
            runningSessionIds={runningSessionIds}
            onSwitchSession={handleSwitchSession}
            onNewSession={handleNewSession}
            agents={agents}
            onCreateSessionWithAgent={handleCreateSessionWithAgent}
            onOpenMarketplace={openMarketplace}
            onOpenKnowledgeBase={openKb}
            activeView={kbOpen ? "kb" : marketplaceOpen ? "extensions" : "chat"}
            chatWelcome={messageCount === 0}
            pendingApprovals={pendingApprovals.length}
            onShowApprovals={() => {}}
            onRenameSession={handleRenameSession}
            onDeleteSession={handleDeleteSession}
            onPinSession={handlePinSession}
          />
        </div>

        {/* Mobile navigation drawer. */}
        {isMobile && mobileNavOpen && (
          <div className="fixed inset-0 z-50 md:hidden">
            <div
              className="absolute inset-0 bg-black/40"
              onClick={() => setMobileNavOpen(false)}
            />
            <div
              className="absolute inset-y-0 left-0 w-72 max-w-[85vw] bg-sidebar shadow-xl"
              style={{
                paddingTop: "env(safe-area-inset-top)",
                paddingBottom: "env(safe-area-inset-bottom)",
              }}
            >
              <Sidebar
                collapsed={false}
                onToggle={() => setMobileNavOpen(false)}
                sessions={sessionItems}
                currentSessionId={transport.getSessionId()}
                runningSessionIds={runningSessionIds}
                onSwitchSession={(id) => {
                  handleSwitchSession(id);
                  setMobileNavOpen(false);
                }}
                onNewSession={() => {
                  handleNewSession();
                  setMobileNavOpen(false);
                }}
                agents={agents}
                onCreateSessionWithAgent={(id) => {
                  handleCreateSessionWithAgent(id);
                  setMobileNavOpen(false);
                }}
                onOpenMarketplace={(type) => {
                  openMarketplace(type);
                  setMobileNavOpen(false);
                }}
                onOpenKnowledgeBase={() => {
                  openKb();
                  setMobileNavOpen(false);
                }}
                activeView={kbOpen ? "kb" : marketplaceOpen ? "extensions" : "chat"}
                chatWelcome={messageCount === 0}
                onRenameSession={handleRenameSession}
                onDeleteSession={handleDeleteSession}
                onPinSession={handlePinSession}
              />
            </div>
          </div>
        )}

        <main className="flex-1 min-w-0 flex flex-col overflow-hidden">
        {/* New-release banner; the settings General tab shows the full controls. */}
        {!settingsOpen && !marketplaceOpen && !kbOpen && <UpdateBanner />}
        {/* First-login cloud guidance (shown once after a successful login). */}
        {!settingsOpen && !marketplaceOpen && !kbOpen && <CloudEnabledBanner />}
        {kbOpen ? (
          <KnowledgeBaseView agents={agents} />
        ) : marketplaceOpen ? (
          <ExtensionsView
            initialType={marketplaceType}
            onSummonExpert={handleCreateSessionWithAgent}
          />
        ) : settingsOpen ? (
          <SettingsPanel
            key={settingsTab}
            transport={transport}
            initialTab={settingsTab}
            onClose={() => setSettingsOpen(false)}
          />
        ) : (previewDocument || workspacePanelOpen) && !isMobile ? (
          <div
            ref={splitContainerRef}
            className="flex flex-row h-full overflow-hidden"
          >
            {/* Left: chat */}
            <div
              className="min-w-0 overflow-hidden flex flex-col"
              style={{ flex: `${1 - previewRatio} 1 0%` }}
            >
              <ChatAppInner key={sessionKey} transport={transport} />
              <GoalPanel />
            </div>
            {/* Resizable divider */}
            <div
              className={`w-1 shrink-0 cursor-col-resize transition-colors ${
                isDragging ? "bg-primary-500" : "bg-transparent hover:bg-primary-400/40"
              }`}
              onMouseDown={(e) => {
                e.preventDefault();
                const container = splitContainerRef.current;
                if (!container) return;
                const rect = container.getBoundingClientRect();
                setIsDragging(true);
                document.body.style.userSelect = "none";
                document.body.style.cursor = "col-resize";

                const onMove = (me: MouseEvent) => {
                  const x = Math.max(0, Math.min(rect.width, me.clientX - rect.left));
                  const ratio = x / rect.width;
                  previewRatioRef.current = 1 - Math.max(0.2, Math.min(0.8, ratio));
                  setPreviewRatio(previewRatioRef.current);
                };
                const onUp = () => {
                  setIsDragging(false);
                  document.body.style.userSelect = "";
                  document.body.style.cursor = "";
                  window.removeEventListener("mousemove", onMove);
                  window.removeEventListener("mouseup", onUp);
                };
                window.addEventListener("mousemove", onMove);
                window.addEventListener("mouseup", onUp);
              }}
            />
            {/* Right: workspace browser or document preview (mutually
                exclusive — enforced by the store setters) */}
            <div
              className="min-w-0 overflow-hidden flex flex-col"
              style={{ flex: `${previewRatio} 1 0%` }}
            >
              {workspacePanelOpen ? (
                <WorkspacePanel
                  key={currentAgent?.id ?? "default"}
                  transport={transport}
                  onClose={() => setWorkspacePanelOpen(false)}
                />
              ) : previewDocument ? (
                <DocumentPreviewPanel
                  document={previewDocument}
                  onClose={() => setPreviewDocument(null)}
                />
              ) : null}
            </div>
          </div>
        ) : (
          <>
            <ChatAppInner key={sessionKey} transport={transport} />
            <GoalPanel />
          </>
        )}
      </main>
      </div>

      {/* Row 3: statusbar (connection/version + run/model). */}
      <Statusbar transport={transport} sidebarCollapsed={sidebarCollapsed} />

      {/* Mobile: document preview takes the whole screen instead of a split. */}
      {isMobile && previewDocument && (
        <div
          className="fixed inset-0 z-30 bg-page flex flex-col"
          style={{
            paddingTop: "env(safe-area-inset-top)",
            paddingBottom: "env(safe-area-inset-bottom)",
          }}
        >
          <DocumentPreviewPanel
            document={previewDocument}
            onClose={() => setPreviewDocument(null)}
          />
        </div>
      )}

      {/* Agent asked a question — the turn is paused until the human answers. */}
      {askPrompt && (
        <AskModal
          prompt={askPrompt}
          onRespond={handleAskRespond}
          onDismiss={handleAskDismiss}
        />
      )}

      {/* A tool call is waiting for human approval — one modal per pending
          approval, front of the queue. */}
      {approvalPrompt && (
        <ApprovalModal
          prompt={approvalPrompt}
          onDecide={handleApprovalDecide}
          onDismiss={handleApprovalDismiss}
        />
      )}
    </div>
  );
}

export default ChatApp;
