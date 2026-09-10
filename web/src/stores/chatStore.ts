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

export type ChipKind = "skill" | "agent" | "connector";

/** Entity chip attached to the composer via the "+" picker. In-memory only;
 *  consumed (and cleared) by the transport when the message is sent. */
export interface ComposerChip {
  kind: ChipKind;
  /** skill name / agent id / connector id */
  id: string;
  /** display label */
  label: string;
  emoji?: string;
  /** connectors only: last known lifecycle state (drives the state dot and
   *  whether a pre-send `connectors.enable` call is needed) */
  state?: "installed" | "enabled" | "disabled" | "error";
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
  /** Entity chips attached via the composer "+" picker; consumed at send. */
  pendingChips: ComposerChip[];

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
  addComposerChip: (chip: ComposerChip) => void;
  removeComposerChip: (kind: ChipKind, id: string) => void;
  setPendingChips: (chips: ComposerChip[]) => void;
  /** Take the current chips and clear them (send-time consumption). */
  consumePendingChips: () => ComposerChip[];
}

export const useChatStore = create<ChatState>((set, get) => ({
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
  pendingChips: [],

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
  addComposerChip: (chip) =>
    set((s) => {
      // Toggle-off when already attached; otherwise dedup by kind+id.
      const exists = s.pendingChips.some((c) => c.kind === chip.kind && c.id === chip.id);
      return {
        pendingChips: exists
          ? s.pendingChips.filter((c) => !(c.kind === chip.kind && c.id === chip.id))
          : [...s.pendingChips, chip],
      };
    }),
  removeComposerChip: (kind, id) =>
    set((s) => ({
      pendingChips: s.pendingChips.filter((c) => !(c.kind === kind && c.id === id)),
    })),
  setPendingChips: (pendingChips) => set({ pendingChips }),
  consumePendingChips: () => {
    const chips = get().pendingChips;
    if (chips.length > 0) set({ pendingChips: [] });
    return chips;
  },
}));
