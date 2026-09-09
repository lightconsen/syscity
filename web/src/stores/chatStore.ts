import { create } from "zustand";
import type { ChatMessage, NetworkStatus } from "@/SyscityWebSocketTransport";

const INTERNALS_KEY = "syscity_internals_visibility";
const VOTES_KEY = "syscity_message_votes";

function loadInternalsVisibility(): Record<string, boolean> {
  try {
    return JSON.parse(localStorage.getItem(INTERNALS_KEY) || "{}");
  } catch {
    return {};
  }
}

function saveInternalsVisibility(v: Record<string, boolean>): void {
  try {
    localStorage.setItem(INTERNALS_KEY, JSON.stringify(v));
  } catch { /* quota exceeded */ }
}

function loadMessageVotes(): Record<string, "up" | "down" | undefined> {
  try {
    return JSON.parse(localStorage.getItem(VOTES_KEY) || "{}");
  } catch {
    return {};
  }
}

function saveMessageVotes(v: Record<string, "up" | "down" | undefined>): void {
  try {
    localStorage.setItem(VOTES_KEY, JSON.stringify(v));
  } catch { /* quota exceeded */ }
}

/** Document shown in the side preview panel. `url` is the artifact's
 *  serving path (owner-addressed); `exportUrl` converts on the server
 *  (e.g. slides canvas → .pptx download). */
interface PreviewDocument {
  filename: string;
  title: string;
  format: string;
  url?: string;
  exportUrl?: string;
}

interface ChatState {
  messages: ChatMessage[];
  sessions: Array<{
    id: string;
    label?: string;
    agent_id?: string;
    pinned?: boolean;
    model?: string | null;
    last_activity?: number;
  }>;
  currentSessionId: string;
  currentAgent?: {
    id: string;
    display_name: string;
    emoji: string;
  };
  networkStatus: NetworkStatus;
  isRunning: boolean;
  runningSessionIds: string[];
  voiceMode: boolean;
  isLoadingHistory: boolean;
  hasMoreHistory: boolean;
  aiInternalsVisibility: Record<string, boolean>;
  /** Vote keyed by stable `turn_id` (survives reloads; the DB vote is the
   *  source of truth, this mirror keeps the selected state visible). */
  messageVotes: Record<string, "up" | "down" | undefined>;
  previewDocument: PreviewDocument | null;
  workspacePanelOpen: boolean;
  /** KB page's right-side document panel — driven by the same Titlebar
   *  "show right sidebar" button, but scoped to the Knowledge Base view. */
  kbPanelOpen: boolean;
  /** Agent summoned from the sidebar while the New Session welcome page is
   *  showing — the session itself is created lazily on the first message. */
  pendingAgent: { id: string; display_name: string; emoji: string } | null;
  /** One-shot composer prefill (expert starter prompt / skill "try it"):
   *  consumed by the composer on the next render, then cleared. */
  pendingDraft: string | null;

  setMessages: (messages: ChatMessage[]) => void;
  prependMessages: (messages: ChatMessage[]) => void;
  appendMessage: (message: ChatMessage) => void;
  updateMessage: (id: string, updater: (msg: ChatMessage) => ChatMessage) => void;
  setSessions: (
    sessions: Array<{
      id: string;
      label?: string;
      agent_id?: string;
      pinned?: boolean;
      model?: string | null;
      last_activity?: number;
    }>
  ) => void;
  setCurrentSessionId: (id: string) => void;
  setCurrentAgent: (agent?: { id: string; display_name: string; emoji: string }) => void;
  setNetworkStatus: (status: NetworkStatus) => void;
  setIsRunning: (running: boolean) => void;
  setRunningSessionIds: (ids: string[]) => void;
  setVoiceMode: (enabled: boolean) => void;
  setIsLoadingHistory: (loading: boolean) => void;
  setHasMoreHistory: (hasMore: boolean) => void;
  setAiInternalsVisibility: (messageId: string, visible: boolean) => void;
  setMessageVote: (turnId: string, vote: "up" | "down" | undefined) => void;
  setPreviewDocument: (doc: PreviewDocument | null) => void;
  setWorkspacePanelOpen: (open: boolean) => void;
  setKbPanelOpen: (open: boolean) => void;
  setPendingAgent: (agent: { id: string; display_name: string; emoji: string } | null) => void;
  setPendingDraft: (draft: string | null) => void;
}

export const useChatStore = create<ChatState>((set) => ({
  messages: [],
  sessions: [],
  currentSessionId: "",
  networkStatus: "connecting",
  isRunning: false,
  runningSessionIds: [],
  voiceMode: false,
  isLoadingHistory: false,
  hasMoreHistory: false,
  aiInternalsVisibility: loadInternalsVisibility(),
  messageVotes: loadMessageVotes(),
  previewDocument: null,
  workspacePanelOpen: false,
  kbPanelOpen: false,
  pendingAgent: null,
  pendingDraft: null,

  setMessages: (messages) => set({ messages }),
  prependMessages: (messages) => set((s) => ({ messages: [...messages, ...s.messages] })),
  appendMessage: (message) => set((s) => ({ messages: [...s.messages, message] })),
  updateMessage: (id, updater) =>
    set((s) => ({
      messages: s.messages.map((m) => (m.id === id ? updater(m) : m)),
    })),
  setSessions: (sessions) => set({ sessions }),
  setCurrentSessionId: (id) => set({ currentSessionId: id }),
  setCurrentAgent: (agent) => set({ currentAgent: agent }),
  setNetworkStatus: (status) => set({ networkStatus: status }),
  setIsRunning: (running) => set({ isRunning: running }),
  setRunningSessionIds: (ids) => set({ runningSessionIds: ids }),
  setVoiceMode: (voiceMode) => set({ voiceMode }),
  setIsLoadingHistory: (isLoadingHistory) => set({ isLoadingHistory }),
  setHasMoreHistory: (hasMoreHistory) => set({ hasMoreHistory }),
  setAiInternalsVisibility: (messageId, visible) =>
    set((s) => {
      const next = {
        ...s.aiInternalsVisibility,
        [messageId]: visible,
      };
      saveInternalsVisibility(next);
      return { aiInternalsVisibility: next };
    }),
  setMessageVote: (turnId, vote) =>
    set((s) => {
      const next = { ...s.messageVotes };
      if (vote === undefined) {
        delete next[turnId];
      } else {
        next[turnId] = vote;
      }
      saveMessageVotes(next);
      return { messageVotes: next };
    }),
  // The workspace panel and document preview share the right-side pane;
  // opening one closes the other.
  setPreviewDocument: (doc) =>
    set(doc ? { previewDocument: doc, workspacePanelOpen: false } : { previewDocument: doc }),
  setWorkspacePanelOpen: (open) =>
    set(open ? { workspacePanelOpen: true, previewDocument: null } : { workspacePanelOpen: false }),
  setKbPanelOpen: (open) => set({ kbPanelOpen: open }),
  setPendingAgent: (pendingAgent) => set({ pendingAgent }),
  setPendingDraft: (pendingDraft) => set({ pendingDraft }),
}));
