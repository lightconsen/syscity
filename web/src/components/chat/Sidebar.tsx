import {
  ChevronLeft,
  ChevronRight,
  Plus,
  Bot,
  Pencil,
  Trash2,
  Pin,
  PinOff,
  Loader2,
  Library,
  Puzzle,
} from "lucide-react";
import { useState, useRef, useCallback, useMemo, useEffect } from "react";
import { useTranslation } from "react-i18next";

interface AgentItem {
  id: string;
  display_name: string;
  emoji: string;
  is_valid: boolean;
  has_heartbeat: boolean;
}

interface SessionItem {
  id: string;
  label?: string;
  pinned?: boolean;
  last_activity?: number;
  agent?: AgentItem;
}

interface SidebarProps {
  collapsed: boolean;
  /** Pending tool-approval count (0 hides the badge). */
  pendingApprovals?: number;
  /** Open the approval modal for the front of the queue. */
  onShowApprovals?: () => void;
  onToggle: () => void;
  sessions: SessionItem[];
  currentSessionId: string;
  runningSessionIds: string[];
  onSwitchSession: (id: string) => void;
  onNewSession: () => void;
  agents: AgentItem[];
  onCreateSessionWithAgent: (agentId: string) => void;
  /** Open the Extensions (marketplace) page, optionally pre-filtered
   * (connector/skill/expert). */
  onOpenMarketplace: (type?: string) => void;
  /** Open the Knowledge Base management page (local + cloud). */
  onOpenKnowledgeBase?: () => void;
  /** Which full-page view is currently showing, so the nav items can
   *  highlight (chat = the main view; kb / extensions are the sidebar's
   *  own pages). */
  activeView?: "chat" | "kb" | "extensions";
  /** The chat view is showing the New Session welcome page (no messages
   *  yet) — highlights "New Session" instead of a session row. */
  chatWelcome?: boolean;
  onRenameSession?: (id: string, name: string) => void | Promise<void>;
  onDeleteSession?: (id: string) => void | Promise<void>;
  onPinSession?: (id: string, pinned: boolean) => void | Promise<void>;
}

export function Sidebar({
  collapsed,
  pendingApprovals = 0,
  onShowApprovals,
  onToggle,
  sessions,
  currentSessionId,
  runningSessionIds,
  onSwitchSession,
  onNewSession,
  agents,
  onCreateSessionWithAgent,
  onOpenMarketplace,
  onOpenKnowledgeBase,
  activeView = "chat",
  chatWelcome = false,
  onRenameSession,
  onDeleteSession,
  onPinSession,
}: SidebarProps) {

  const { t } = useTranslation("chat");
  const listContainerRef = useRef<HTMLDivElement>(null);
  // Session rows only read as "current" while a real conversation is on
  // screen — on the welcome page, KB, or Extensions the nav item takes over.
  const chatActive = activeView === "chat" && !chatWelcome;
  const [sessionRatio, setSessionRatio] = useState<number>(() => {
    const saved = localStorage.getItem("syscity_sidebar_session_ratio");
    if (saved) {
      const v = parseFloat(saved);
      if (!isNaN(v)) return Math.max(0.2, Math.min(0.8, v));
    }
    return 0.6;
  });
  const [isDragging, setIsDragging] = useState(false);

  useEffect(() => {
    localStorage.setItem("syscity_sidebar_session_ratio", String(sessionRatio));
  }, [sessionRatio]);

  const handleMouseDown = useCallback(
    (e: React.MouseEvent<HTMLDivElement>) => {
      if (collapsed) return;
      e.preventDefault();
      setIsDragging(true);
      document.body.style.userSelect = "none";

      const container = listContainerRef.current;
      if (!container) return;
      const rect = container.getBoundingClientRect();

      const handleMouseMove = (moveEvent: MouseEvent) => {
        const y = moveEvent.clientY - rect.top;
        const ratio = Math.max(0.15, Math.min(0.85, y / rect.height));
        setSessionRatio(ratio);
      };

      const handleMouseUp = () => {
        setIsDragging(false);
        document.body.style.userSelect = "";
        window.removeEventListener("mousemove", handleMouseMove);
        window.removeEventListener("mouseup", handleMouseUp);
      };

      window.addEventListener("mousemove", handleMouseMove);
      window.addEventListener("mouseup", handleMouseUp);
    },
    [collapsed]
  );

  const groups = useMemo(() => {
    const now = new Date();
    const startOfToday = new Date(
      now.getFullYear(),
      now.getMonth(),
      now.getDate()
    ).getTime();

    const pinned = sessions
      .filter((s) => s.pinned)
      .sort((a, b) => (b.last_activity || 0) - (a.last_activity || 0));

    const buckets = new Map<string, SessionItem[]>();
    for (const s of sessions) {
      if (s.pinned) continue;
      const ts = s.last_activity || 0;
      const d = new Date(ts);
      const dayStart = new Date(
        d.getFullYear(),
        d.getMonth(),
        d.getDate()
      ).getTime();
      const diffDays = Math.floor(
        (startOfToday - dayStart) / (1000 * 60 * 60 * 24)
      );
      let key: string;
      if (diffDays <= 0) {
        key = "Sidebar.today";
      } else if (diffDays === 1) {
        key = "Sidebar.yesterday";
      } else if (diffDays <= 7) {
        key = "Sidebar.last7Days";
      } else if (diffDays <= 30) {
        key = "Sidebar.last30Days";
      } else {
        key = "Sidebar.older";
      }
      if (!buckets.has(key)) {
        buckets.set(key, []);
      }
      buckets.get(key)!.push(s);
    }

    const order = ["Sidebar.today", "Sidebar.yesterday", "Sidebar.last7Days", "Sidebar.last30Days", "Sidebar.older"];
    const result: { label: string; sessions: SessionItem[] }[] = [];
    if (pinned.length > 0) {
      result.push({ label: "Sidebar.pinned", sessions: pinned });
    }
    for (const key of order) {
      const list = buckets.get(key);
      if (list && list.length > 0) {
        list.sort((a, b) => (b.last_activity || 0) - (a.last_activity || 0));
        result.push({ label: key, sessions: list });
      }
    }
    return result;
  }, [sessions]);

  return (
    <aside
      className={`shrink-0 h-full flex flex-col bg-rail transition-all duration-300 overflow-x-hidden ${
        collapsed ? "w-16" : "w-64"
      }`}
    >
      {/* Top: Logo + Name + Collapse — mobile drawer only; on desktop the
          header lives in the Titlebar's sidebar-width zone. */}
      <div className="h-14 md:hidden flex items-center justify-between px-3 shrink-0">
        <div className="flex items-center gap-2 overflow-hidden">
          <img
            src="/syscity.png"
            alt="Syscity"
            className="w-6 h-6 shrink-0"
            draggable={false}
          />
          {!collapsed && (
            <span className="text-sm font-semibold text-primary whitespace-nowrap">
              Syscity
            </span>
          )}
        </div>
        <button
          onClick={onToggle}
          className="p-1 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition shrink-0"
          title={collapsed ? t("Sidebar.expand") : t("Sidebar.collapse")}
          aria-label={collapsed ? t("Sidebar.expand") : t("Sidebar.collapse")}
        >
          {collapsed ? (
            <ChevronRight className="w-4 h-4" />
          ) : (
            <ChevronLeft className="w-4 h-4" />
          )}
        </button>
      </div>

      {/* Pending tool approvals — a persistent reminder above the session
          list. Clicking opens the approval modal for the front of the queue. */}
      {!collapsed && pendingApprovals > 0 && (
        <button
          onClick={onShowApprovals}
          className="mx-1 mb-2 px-3 py-2 rounded-lg text-xs font-medium bg-red-100 dark:bg-red-900/30 text-red-700 dark:text-red-300 hover:bg-red-200 dark:hover:bg-red-900/40 transition text-left shrink-0"
          title={t("Sidebar.showPendingApprovals")}
        >
          ⚠️ {t("Sidebar.pendingApprovals", { count: pendingApprovals })}
        </button>
      )}

      <div
        ref={listContainerRef}
        className="flex-1 flex flex-col min-h-0 overflow-hidden"
      >
        {/* Sessions */}
        <div
          className="scrollbar-hover overflow-y-auto overflow-x-hidden px-1 py-2"
          style={{ flexBasis: `${sessionRatio * 100}%`, flexShrink: 0, flexGrow: 0 }}
          role="list"
        >
          <button
            onClick={onNewSession}
            className={`w-full text-left px-3 py-1.5 mb-0.5 rounded-lg text-sm transition flex items-center gap-2 ${
              activeView === "chat" && chatWelcome
                ? "bg-primary-100 dark:bg-primary-900/20 text-primary"
                : "text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
            } ${collapsed ? "justify-center" : ""}`}
            title={t("Sidebar.newSession")}
            aria-label={t("Sidebar.newSession")}
            aria-current={activeView === "chat" && chatWelcome ? "page" : undefined}
          >
            <Plus className="w-4 h-4 shrink-0" />
            {!collapsed && <span>{t("Sidebar.newSessionLabel")}</span>}
          </button>
          {/* Extensions (marketplace): connectors, skills, experts. */}
          <button
            onClick={() => onOpenMarketplace()}
            className={`w-full text-left px-3 py-1.5 mb-0.5 rounded-lg text-sm transition flex items-center gap-2 ${
              activeView === "extensions"
                ? "bg-primary-100 dark:bg-primary-900/20 text-primary"
                : "text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
            } ${collapsed ? "justify-center" : ""}`}
            title={t("Sidebar.browseExtensions")}
            aria-label={t("Sidebar.extensions")}
            aria-current={activeView === "extensions" ? "page" : undefined}
          >
            <Puzzle className="w-4 h-4 shrink-0" />
            {!collapsed && <span>{t("Sidebar.extensions")}</span>}
          </button>
          {/* Knowledge Base: local per-agent collections + cloud KBs. */}
          {onOpenKnowledgeBase && (
            <button
              onClick={onOpenKnowledgeBase}
              className={`w-full text-left px-3 py-1.5 mb-0.5 rounded-lg text-sm transition flex items-center gap-2 ${
                activeView === "kb"
                  ? "bg-primary-100 dark:bg-primary-900/20 text-primary"
                  : "text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
              } ${collapsed ? "justify-center" : ""}`}
              title={t("Sidebar.manageKnowledgeBases")}
              aria-label={t("Sidebar.knowledgeBase")}
              aria-current={activeView === "kb" ? "page" : undefined}
            >
              <Library className="w-4 h-4 shrink-0" />
              {!collapsed && <span>{t("Sidebar.knowledgeBase")}</span>}
            </button>
          )}
          {!collapsed &&
            groups.map((group) => (
              <div key={group.label} className="mb-2">
                <div className="px-3 pb-1">
                  <span className="text-[10px] uppercase tracking-wider text-secondary font-medium">
                    {t(group.label)}
                  </span>
                </div>
                {group.sessions.map((s) => (
                  <SessionRow
                    key={s.id}
                    session={s}
                    currentSessionId={chatActive ? currentSessionId : ""}
                    runningSessionIds={runningSessionIds}
                    collapsed={collapsed}
                    onSwitch={() => onSwitchSession(s.id)}
                    onRename={onRenameSession}
                    onDelete={onDeleteSession}
                    onPin={onPinSession}
                  />
                ))}
              </div>
            ))}
          {collapsed &&
            sessions.map((s) => (
              <SessionRow
                key={s.id}
                session={s}
                currentSessionId={chatActive ? currentSessionId : ""}
                runningSessionIds={runningSessionIds}
                collapsed={collapsed}
                onSwitch={() => onSwitchSession(s.id)}
                onRename={onRenameSession}
                onDelete={onDeleteSession}
                onPin={onPinSession}
              />
            ))}
        </div>

        {/* Resize handle */}
        {!collapsed && (
          <div
            onMouseDown={handleMouseDown}
            className={`relative h-1 shrink-0 cursor-ns-resize bg-transparent hover:bg-primary-600/10 transition-colors ${
              isDragging ? "bg-primary-600/20" : ""
            }`}
            role="separator"
            aria-orientation="horizontal"
            aria-label={t("Sidebar.resizePanels")}
            title={t("Sidebar.dragToResize")}
          >
            <div className="absolute left-1/2 -translate-x-1/2 top-1/2 -translate-y-1/2 w-8 h-0.5 rounded-full bg-subtle" />
          </div>
        )}

        {/* Agents */}
        <div
          className="flex flex-col min-h-0 border-t border-subtle"
          style={{
            flexBasis: `${(1 - sessionRatio) * 100}%`,
            flexShrink: 0,
            flexGrow: 0,
          }}
        >
          {!collapsed && (
            <div className="px-3 py-2 shrink-0">
              <span className="text-[10px] uppercase tracking-wider text-secondary font-medium">
                {t("Sidebar.agents")}
              </span>
            </div>
          )}
          <div className="scrollbar-hover flex-1 overflow-y-auto overflow-x-hidden px-1 py-1" role="list">
            {agents.length === 0 && !collapsed && (
              <div className="px-3 py-4 text-xs text-secondary text-center">
                <Bot className="w-5 h-5 mx-auto mb-2 opacity-50" />
                <p>{t("Sidebar.noAgents")}</p>
                <p className="mt-1 opacity-70">
                  {t("Sidebar.createAgentHint")}
                </p>
              </div>
            )}
            {agents.map((agent) => (
              <button
                key={agent.id}
                onClick={() => onCreateSessionWithAgent(agent.id)}
                className={`w-full text-left px-3 py-2 rounded-lg text-sm transition flex items-center gap-2 text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04] ${
                  collapsed ? "justify-center" : ""
                }`}
                title={t("Sidebar.newSessionWith", { name: agent.display_name })}
                role="listitem"
              >
                <span className="text-base shrink-0" aria-hidden="true">
                  {agent.emoji}
                </span>
                {!collapsed && (
                  <>
                    <span className="truncate flex-1 min-w-0">
                      {agent.display_name}
                    </span>
                  </>
                )}
              </button>
            ))}
          </div>
        </div>
      </div>

    </aside>
  );
}

interface SessionRowProps {
  session: SessionItem;
  currentSessionId: string;
  runningSessionIds: string[];
  collapsed: boolean;
  onSwitch: () => void;
  onRename?: (id: string, name: string) => void | Promise<void>;
  onDelete?: (id: string) => void | Promise<void>;
  onPin?: (id: string, pinned: boolean) => void | Promise<void>;
}

function SessionRow({
  session,
  currentSessionId,
  runningSessionIds,
  collapsed,
  onSwitch,
  onRename,
  onDelete,
  onPin,
}: SessionRowProps) {
  const { t } = useTranslation("chat");
  const isActive = session.id === currentSessionId;
  const displayName = session.label || t("Sidebar.untitled");
  const [isEditing, setIsEditing] = useState(false);
  const [editName, setEditName] = useState(displayName);
  const inputRef = useRef<HTMLInputElement>(null);

  const handleRename = useCallback(() => {
    const trimmed = editName.trim();
    if (trimmed && trimmed !== displayName && onRename) {
      onRename(session.id, trimmed);
    }
    setIsEditing(false);
  }, [editName, displayName, onRename, session.id]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLInputElement>) => {
      if (e.key === "Enter") {
        e.preventDefault();
        handleRename();
      } else if (e.key === "Escape") {
        setIsEditing(false);
        setEditName(displayName);
      }
    },
    [handleRename, displayName]
  );

  const handleBlur = useCallback(() => {
    setTimeout(() => {
      if (document.activeElement !== inputRef.current) {
        handleRename();
      }
    }, 150);
  }, [handleRename]);

  const handleDelete = useCallback(() => {
    if (!onDelete) return;
    if (confirm(t("Sidebar.deleteConfirm", { name: displayName }))) {
      onDelete(session.id);
    }
  }, [displayName, onDelete, session.id]);

  const handlePin = useCallback(() => {
    if (!onPin) return;
    onPin(session.id, !session.pinned);
  }, [onPin, session.id, session.pinned]);

  if (collapsed) {
    return (
      <button
        onClick={onSwitch}
        className={`w-full text-left px-3 py-2 rounded-lg text-sm transition flex items-center justify-center ${
          isActive
            ? "bg-primary-100 dark:bg-primary-900/20 text-primary"
            : "text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
        }`}
        title={session.agent ? `${session.agent.emoji} ${session.agent.display_name} — ${displayName}` : displayName}
        aria-label={displayName}
        role="listitem"
      >
        <span className="text-base shrink-0" aria-hidden="true">
          {session.agent?.emoji || "💬"}
        </span>
      </button>
    );
  }

  if (isEditing) {
    return (
      <div className="px-3 py-1.5">
        <input
          ref={inputRef}
          type="text"
          value={editName}
          onChange={(e) => setEditName(e.target.value)}
          onKeyDown={handleKeyDown}
          onBlur={handleBlur}
          autoFocus
          className="w-full text-sm px-2 py-1 rounded-md bg-card text-primary border border-subtle focus:outline-none focus:ring-2 focus:ring-primary-500/20"
        />
      </div>
    );
  }

  return (
    <div
      className={`group flex items-center gap-1 px-1 py-0.5 rounded-lg transition ${
        isActive
          ? "bg-primary-100 dark:bg-primary-900/20"
          : "hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
      }`}
      role="listitem"
    >
      <button
        onClick={onSwitch}
        className={`flex-1 min-w-0 text-left px-2 py-1.5 rounded-md text-sm flex items-center gap-2 transition ${
          isActive ? "text-primary" : "text-secondary"
        }`}
        title={displayName}
      >
        <span className="text-base shrink-0" aria-hidden="true">
          {session.agent?.emoji || "💬"}
        </span>
        <span className="truncate flex-1 min-w-0">{displayName}</span>
      </button>
      {(onPin || onRename || onDelete) && (
        <div className="relative flex items-center gap-0.5 pr-1 ml-auto">
          {/* Three action buttons — always in DOM for spacing, visible on hover */}
          <div className="flex items-center gap-0.5 opacity-0 group-hover:opacity-100 transition-opacity">
            {onPin && (
              <button
                onClick={handlePin}
                className={`p-1 rounded-md transition ${
                  session.pinned
                    ? "text-primary hover:text-primary"
                    : "text-secondary hover:text-primary"
                } hover:bg-black/5 dark:hover:bg-white/5`}
                title={session.pinned ? t("Sidebar.unpin") : t("Sidebar.pin")}
                aria-label={session.pinned ? t("Sidebar.unpin") : t("Sidebar.pin")}
              >
                {session.pinned ? (
                  <PinOff className="w-3 h-3" />
                ) : (
                  <Pin className="w-3 h-3" />
                )}
              </button>
            )}
            {onRename && (
              <button
                onClick={() => {
                  setEditName(displayName);
                  setIsEditing(true);
                }}
                className="p-1 rounded-md text-secondary hover:text-primary hover:bg-black/5 dark:hover:bg-white/5 transition"
                title={t("Sidebar.rename")}
                aria-label={t("Sidebar.rename")}
              >
                <Pencil className="w-3 h-3" />
              </button>
            )}
            {onDelete && (
              <button
                onClick={handleDelete}
                className="p-1 rounded-md text-secondary hover:text-red-500 hover:bg-red-500/10 transition"
                title={t("Sidebar.deleteSession")}
                aria-label={t("Sidebar.deleteSession")}
              >
                <Trash2 className="w-3 h-3" />
              </button>
            )}
          </div>
          {/* Loading overlay — covers same area as buttons, hidden on hover */}
          {runningSessionIds.includes(session.id) && (
            <div className="absolute inset-0 flex items-center justify-end pr-0.5 opacity-100 group-hover:opacity-0 transition-opacity pointer-events-none">
              <Loader2 className="w-4 h-4 animate-spin text-blue-400 shrink-0" />
            </div>
          )}
        </div>
      )}
    </div>
  );
}
