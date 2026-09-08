import { MarkdownMessage } from "@/components/shared/MarkdownMessage";
import { ReasoningPart } from "@/components/shared/ReasoningPart";
import { ToolCallPart } from "@/components/shared/ToolCallPart";
import { DocumentRefPart } from "@/components/shared/DocumentRefPart";
import { Avatar } from "./Avatar";
import { LiveStatusBar } from "./LiveStatusBar";
import {
  AlertTriangle,
  Clock,
  Wrench,
  Copy,
  Check,
  Pencil,
  RotateCcw,
  ChevronDown,
  ChevronUp,
  BrainCircuit,
  ThumbsUp,
  ThumbsDown,
  Coins,
} from "lucide-react";
import { formatDuration } from "@/lib/utils";
import { cloudStatus } from "@/lib/cloud";
import type { ChatMessage, SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { useState, useCallback, useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { useChatStore } from "@/stores/chatStore";

interface MessageBubbleProps {
  message: ChatMessage;
  transport?: SyscityWebSocketTransport;
  onEdit?: (id: string, text: string) => void;
}

const centerStyle: Record<string, string> = {
  paddingLeft: "calc((100% - var(--message-list-max-width)) / 2)",
  paddingRight: "calc((100% - var(--message-list-max-width)) / 2)",
};

// Console deep link for the insufficient-credits recharge button (cached —
// it never changes for the lifetime of the page).
let cachedConsoleUrl: string | null | undefined;
function fetchConsoleUrl(): Promise<string | null> {
  if (cachedConsoleUrl !== undefined) return Promise.resolve(cachedConsoleUrl);
  return cloudStatus()
    .then((s) => (cachedConsoleUrl = s.console_url?.replace(/\/$/, "") ?? null))
    .catch(() => (cachedConsoleUrl = null));
}

/** A3: dedicated card when the cloud relay rejected the turn for overdraft. */
function InsufficientCreditsCard() {
  const { t } = useTranslation("chat");
  const [consoleUrl, setConsoleUrl] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    fetchConsoleUrl().then((u) => {
      if (alive) setConsoleUrl(u);
    });
    return () => {
      alive = false;
    };
  }, []);
  return (
    <div className="mt-1 rounded-lg border border-red-300/60 dark:border-red-500/30 bg-red-50 dark:bg-red-900/20 px-3 py-2.5 max-w-xl">
      <div className="flex items-center gap-1.5 text-xs font-medium text-red-600 dark:text-red-400">
        <AlertTriangle className="w-3.5 h-3.5" />
        {t("MessageBubble.insufficientCredits")}
      </div>
      <p className="mt-1 text-xs text-secondary">
        {t("MessageBubble.insufficientCreditsHint")}
      </p>
      {consoleUrl && (
        <a
          href={consoleUrl}
          target="_blank"
          rel="noreferrer"
          className="mt-2 inline-flex items-center gap-1 px-2.5 py-1 rounded-md bg-primary-500 hover:bg-primary-600 text-white text-xs font-medium transition"
        >
          <Coins className="w-3 h-3" />
          {t("MessageBubble.recharge")}
        </a>
      )}
    </div>
  );
}

function useCopied() {
  const [copied, setCopied] = useState(false);
  const copy = useCallback(async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
      return true;
    } catch {
      return false;
    }
  }, []);
  return { copied, copy };
}

function ActionButton({
  icon: Icon,
  title,
  onClick,
  active,
}: {
  icon: React.ComponentType<{ className?: string }>;
  title: string;
  onClick: () => void;
  active?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      aria-label={title}
      className={`p-1 rounded-md transition ${
        active
          ? "text-primary-600 dark:text-primary-400 bg-primary-100 dark:bg-primary-900/20"
          : "text-secondary hover:text-primary hover:bg-black/5 dark:hover:bg-white/5"
      }`}
    >
      <Icon className="w-3.5 h-3.5" />
    </button>
  );
}

function UserMessageActions({
  content,
  onEdit,
}: {
  content: string;
  onEdit?: () => void;
}) {
  const { copied, copy } = useCopied();
  const { t } = useTranslation("chat");

  return (
    <div className="flex items-center justify-end gap-1 mt-1 opacity-0 group-hover:opacity-100 transition-opacity">
      <ActionButton
        icon={copied ? Check : Copy}
        title={copied ? t("MessageBubble.copied") : t("MessageBubble.copy")}
        onClick={() => copy(content)}
        active={copied}
      />
      {onEdit && (
        <ActionButton icon={Pencil} title={t("MessageBubble.edit")} onClick={onEdit} />
      )}
    </div>
  );
}

function AssistantMessageActions({
  content,
  onRegenerate,
  turnId,
  vote,
  onVote,
}: {
  content: string;
  onRegenerate?: () => void;
  turnId?: string;
  vote?: "up" | "down" | undefined;
  onVote?: (vote: "up" | "down") => void;
}) {
  const { copied, copy } = useCopied();
  const { t } = useTranslation("chat");

  return (
    <div className="flex items-center gap-1 mt-1 opacity-0 group-hover:opacity-100 transition-opacity">
      <ActionButton
        icon={copied ? Check : Copy}
        title={copied ? t("MessageBubble.copied") : t("MessageBubble.copy")}
        onClick={() => copy(content)}
        active={copied}
      />
      {turnId && onVote && (
        <>
          <ActionButton
            icon={ThumbsUp}
            title={t("MessageBubble.helpful")}
            onClick={() => onVote("up")}
            active={vote === "up"}
          />
          <ActionButton
            icon={ThumbsDown}
            title={t("MessageBubble.notHelpful")}
            onClick={() => onVote("down")}
            active={vote === "down"}
          />
        </>
      )}
      {onRegenerate && (
        <ActionButton
          icon={RotateCcw}
          title={t("MessageBubble.regenerate")}
          onClick={onRegenerate}
        />
      )}
    </div>
  );
}

/** Extract only the final assistant text, ignoring reasoning/tool-call parts. */
function assistantReplyText(message: ChatMessage): string {
  if (!message.parts || message.parts.length === 0) return message.content;
  const textParts = message.parts
    .filter((part) => part.type === "text")
    .map((part) => part.text || "");
  return textParts.join("\n\n");
}

function countInternalParts(message: ChatMessage): {
  reasoning: number;
  toolCalls: number;
} {
  if (!message.parts) return { reasoning: 0, toolCalls: 0 };
  return message.parts.reduce(
    (acc, part) => {
      if (part.type === "reasoning") acc.reasoning += 1;
      if (part.type === "tool-call") acc.toolCalls += 1;
      return acc;
    },
    { reasoning: 0, toolCalls: 0 }
  );
}

function InternalsToggle({
  reasoning,
  toolCalls,
  expanded,
  onToggle,
}: {
  reasoning: number;
  toolCalls: number;
  expanded: boolean;
  onToggle: () => void;
}) {
  const { t } = useTranslation("chat");
  if (reasoning === 0 && toolCalls === 0) return null;

  const parts: string[] = [];
  if (reasoning > 0) parts.push(t("MessageBubble.thinkingCount", { count: reasoning }));
  if (toolCalls > 0) parts.push(t("MessageBubble.toolCount", { count: toolCalls }));

  return (
    <button
      type="button"
      onClick={onToggle}
      className="flex items-center gap-1.5 text-[11px] text-secondary hover:text-primary transition mb-1.5"
      aria-expanded={expanded}
    >
      <BrainCircuit className="w-3.5 h-3.5" />
      <span>{parts.join(" · ")}</span>
      {expanded ? (
        <ChevronUp className="w-3 h-3" />
      ) : (
        <ChevronDown className="w-3 h-3" />
      )}
    </button>
  );
}

export function MessageBubble({ message, transport, onEdit }: MessageBubbleProps) {
  const { t } = useTranslation("chat");
  const isUser = message.role === "user";
  const [isEditing, setIsEditing] = useState(false);
  const [editText, setEditText] = useState(message.content);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  const handleEditSubmit = useCallback(() => {
    const trimmed = editText.trim();
    if (trimmed && trimmed !== message.content && onEdit) {
      onEdit(message.id, trimmed);
    }
    setIsEditing(false);
  }, [editText, message.content, message.id, onEdit]);

  const handleEditKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        handleEditSubmit();
      } else if (e.key === "Escape") {
        setIsEditing(false);
        setEditText(message.content);
      }
    },
    [handleEditSubmit, message.content]
  );

  const handleBlur = useCallback(() => {
    // Small delay so clicks on other elements can be processed first
    setTimeout(() => {
      if (document.activeElement !== textareaRef.current) {
        handleEditSubmit();
      }
    }, 150);
  }, [handleEditSubmit]);

  if (isUser) {
    return (
      <div className="py-4 group">
        <div className="flex gap-3 flex-row-reverse" style={centerStyle}>
          <Avatar role="user" />
          <div className="flex-1 min-w-0 text-right">
            <div className="text-[11px] font-medium text-secondary mb-1 uppercase tracking-wide">
              {t("MessageBubble.you")}
            </div>
            {isEditing ? (
              <div className="inline-block text-left w-full max-w-xl">
                <textarea
                  ref={textareaRef}
                  value={editText}
                  onChange={(e) => setEditText(e.target.value)}
                  onKeyDown={handleEditKeyDown}
                  onBlur={handleBlur}
                  autoFocus
                  rows={Math.min(6, editText.split("\n").length + 1)}
                  className="w-full resize-none rounded-xl px-4 py-2.5 text-sm bg-card text-primary border border-subtle focus:outline-none focus:ring-2 focus:ring-primary-500/20"
                />
                <div className="mt-1 text-[10px] text-secondary text-right">
                  {t("MessageBubble.enterToSave")}
                </div>
              </div>
            ) : (
              <>
                <div className="inline-block text-left rounded-2xl px-4 py-2.5 bg-primary-600 text-white rounded-br-md">
                  <p className="text-sm leading-relaxed whitespace-pre-wrap">
                    {message.content}
                  </p>
                </div>
                <UserMessageActions
                  content={message.content}
                  onEdit={() => {
                    setEditText(message.content);
                    setIsEditing(true);
                  }}
                />
              </>
            )}
          </div>
        </div>
      </div>
    );
  }

  const hasParts = message.parts && message.parts.length > 0;
  const isAssistant = message.role === "assistant";
  const hasMetadata =
    isAssistant &&
    (message.durationMs !== undefined ||
      message.toolCount !== undefined ||
      message.credits !== undefined);
  const replyText = assistantReplyText(message);
  const internalCounts = countInternalParts(message);
  const hasInternals = internalCounts.reasoning > 0 || internalCounts.toolCalls > 0;
  const showInternals = useChatStore((s) => s.aiInternalsVisibility[message.id] ?? false);
  const setAiInternalsVisibility = useChatStore((s) => s.setAiInternalsVisibility);
  const toggleInternals = useCallback(() => {
    setAiInternalsVisibility(message.id, !showInternals);
  }, [message.id, showInternals, setAiInternalsVisibility]);

  const handleRegenerate = useCallback(() => {
    transport?.regenerateAssistantMessage(message.id);
  }, [message.id, transport]);

  const turnId = message.turnId;
  const vote = useChatStore((s) => (turnId ? s.messageVotes[turnId] : undefined));

  const handleVote = useCallback(
    async (v: "up" | "down") => {
      if (!turnId) return;
      const state = useChatStore.getState();
      const current = state.messageVotes[turnId];
      const next = current === v ? undefined : v;
      // Optimistic local mirror first for snappy UI.
      state.setMessageVote(turnId, next);
      if (!next || !transport) return; // toggle-off: no clear API on the DB
      // Reconstruct the user input for this turn (for badcase dedup seeding).
      let input: string | undefined;
      const idx = state.messages.findIndex((m) => m.id === message.id);
      for (let i = idx - 1; i >= 0; i--) {
        if (state.messages[i].role === "user") {
          input = state.messages[i].content;
          break;
        }
      }
      const ok = await transport.vote(turnId, next, { input, response: replyText });
      if (!ok) {
        // Revert on failure so the UI never diverges from the DB.
        useChatStore.getState().setMessageVote(turnId, current);
      }
    },
    [turnId, transport, message.id, replyText]
  );

  return (
    <div className="py-4 group">
      <div className="flex gap-3 flex-row" style={centerStyle}>
        <Avatar role="assistant" />
        <div className="flex-1 min-w-0">
          <div className="text-[11px] font-medium text-secondary mb-1 uppercase tracking-wide">
            Syscity
          </div>
          {hasInternals && (
            <InternalsToggle
              reasoning={internalCounts.reasoning}
              toolCalls={internalCounts.toolCalls}
              expanded={showInternals}
              onToggle={toggleInternals}
            />
          )}
          {message.errorCode === "insufficient_credits" ? (
            <InsufficientCreditsCard />
          ) : hasParts ? (
            <div className="space-y-1">
              {/* Internals panel: reasoning + tool-calls with Collapse button */}
              {hasInternals && (
                <div className={showInternals ? "" : "hidden"}>
                  <div className="space-y-1">
                    {message.parts!.map((part, i) => {
                      if (part.type === "reasoning") {
                        return (
                          <div key={i}>
                            <ReasoningPart text={part.text || ""} nonCollapsible />
                          </div>
                        );
                      }
                      if (part.type === "tool-call") {
                        return (
                          <div key={i}>
                            <ToolCallPart
                              toolName={part.toolName || "tool"}
                              args={part.args || {}}
                              result={part.result}
                              data={part.data}
                              transport={transport}
                              nonCollapsible
                            />
                          </div>
                        );
                      }
                      return null;
                    })}
                  </div>
                </div>
              )}
              {/* Text and document-ref parts (always visible) */}
              {message.parts!.map((part, i) => {
                if (part.type === "text") {
                  return (
                    <div key={i} className="text-primary">
                      <MarkdownMessage text={part.text || ""} />
                    </div>
                  );
                }
                if (part.type === "document-ref") {
                  const docData = (part as any).data;
                  if (!docData) return null;
                  return <DocumentRefPart key={i} data={docData} />;
                }
                return null;
              })}
            </div>
          ) : (
            <div className="text-primary">
              <MarkdownMessage text={message.content} />
            </div>
          )}
          {/* Live status or metadata footer */}
          {message.liveStatus && (
            <LiveStatusBar
              liveStatus={message.liveStatus}
              startTime={message.timestamp ?? Date.now() - 5000}
            />
          )}
          {!message.liveStatus && hasMetadata && (
            <div className="mt-1.5 flex items-center gap-3 text-[10px] text-secondary">
              {message.durationMs !== undefined && (
                <span className="flex items-center gap-1">
                  <Clock className="w-3 h-3" />
                  {formatDuration(message.durationMs)}
                </span>
              )}
              {message.toolCount !== undefined && message.toolCount > 0 && (
                <span className="flex items-center gap-1">
                  <Wrench className="w-3 h-3" />
                  {t("MessageBubble.toolCount", { count: message.toolCount })}
                </span>
              )}
              {/* A1: cloud credit metering for this turn */}
              {message.credits !== undefined && (
                <span className="flex items-center gap-1">
                  <Coins className="w-3 h-3" />
                  {t("MessageBubble.creditsUsed", { n: message.credits })}
                  {message.balanceAfter !== undefined &&
                    " " + t("MessageBubble.creditsBalance", { balance: message.balanceAfter })}
                </span>
              )}
            </div>
          )}
          {!message.liveStatus && (
            <AssistantMessageActions
              content={replyText}
              onRegenerate={handleRegenerate}
              turnId={turnId}
              vote={vote}
              onVote={handleVote}
            />
          )}
        </div>
      </div>
    </div>
  );
}
