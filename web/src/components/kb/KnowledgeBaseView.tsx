import { useCallback, useEffect, useRef, useState, type CSSProperties } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { Cloud, CloudUpload, FileText, Loader2, RefreshCw, Trash2, Upload, X } from "lucide-react";
import { getActiveTransport } from "@/SyscityWebSocketTransport";
import { cloudLoginUrl, cloudStatus, type CloudStatus } from "@/lib/cloud";
import { useChatStore } from "@/stores/chatStore";
import { MarkdownMessage } from "@/components/shared/MarkdownMessage";

export interface KbAgent {
  id: string;
  display_name: string;
  emoji: string;
  is_valid: boolean;
  has_heartbeat: boolean;
}

interface CollectionSummary {
  collection: string;
  total_docs: number;
  total_chunks: number;
  last_indexed_at: string | null;
  stale_count: number;
  failed_count: number;
}

interface KbDoc {
  doc_id: string;
  source_id: string;
  chunk_count: number;
  status: string;
  error: string | null;
  indexed_at: string;
}

/** One flat row across all collections, with the owning collection attached. */
type DocRow = KbDoc & { collection: string };

interface CloudKb {
  id: string;
  name: string;
  document_count?: number;
  created_at?: string;
}

interface CloudKbDoc {
  filename: string;
  status: string;
}

interface PushResult {
  total: number;
  pushed: number;
  unchanged: number;
  skipped_url: number;
  skipped_external: number;
  too_large: number;
  failed: number;
  errors: string[];
}

interface PullResult {
  total: number;
  pulled: number;
  unchanged: number;
  failed: number;
  errors: string[];
}

/** Upload cap mirrored from the gateway (`kb.ingest`: 32 MiB of raw bytes). */
const MAX_UPLOAD_BYTES = 32 * 1024 * 1024;
const ALLOWED_EXT = ["pdf", "docx", "xlsx", "txt", "md"];
const DEFAULT_AGENT = "default";
const LOGIN_TIMEOUT_MS = 180_000;
const POLL_MS = 1_500;

const statusBadge = (status: string) => {
  switch (status) {
    case "indexed":
      return "bg-green-100 dark:bg-green-900/30 text-green-700 dark:text-green-400";
    case "stale":
      return "bg-amber-100 dark:bg-amber-900/30 text-amber-700 dark:text-amber-400";
    case "failed":
      return "bg-red-100 dark:bg-red-900/30 text-red-700 dark:text-red-400";
    default:
      return "bg-black/5 dark:bg-white/10 text-secondary";
  }
};

/** Display owner for a collection: the agent's name for `kb-{agent_id}`,
 * "Default" for collections not bound to an agent (shared). */
function ownerOf(
  collection: string,
  agents: KbAgent[],
  t: TFunction
): { label: string; emoji: string } {
  if (collection.startsWith("kb-") && collection.length > 3) {
    const id = collection.slice(3);
    const a = agents.find((x) => x.id === id);
    if (a) return { label: a.display_name, emoji: a.emoji };
    return { label: id, emoji: "🤖" };
  }
  return { label: t("KnowledgeBaseView.defaultCollection"), emoji: "📁" };
}

/** File → base64 (same approach as AddSkillForm: no chunking, small files). */
async function fileToBase64(file: File): Promise<string> {
  const bytes = new Uint8Array(await file.arrayBuffer());
  let binary = "";
  for (let i = 0; i < bytes.byteLength; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary);
}

/** Human summary of a push/pull response. */
function resultSummary(r: PushResult | PullResult, verb: string, t: TFunction): string {
  const parts = [
    t("KnowledgeBaseView.summaryVerbCount", { verb, n: "pushed" in r ? r.pushed : r.pulled }),
    t("KnowledgeBaseView.summaryUnchanged", { n: r.unchanged }),
  ];
  if ("skipped_url" in r && r.skipped_url > 0)
    parts.push(t("KnowledgeBaseView.summaryUrl", { n: r.skipped_url }));
  if ("skipped_external" in r && r.skipped_external > 0)
    parts.push(t("KnowledgeBaseView.summaryExternal", { n: r.skipped_external }));
  if ("too_large" in r && r.too_large > 0)
    parts.push(t("KnowledgeBaseView.summaryTooLarge", { n: r.too_large }));
  if (r.failed > 0) parts.push(t("KnowledgeBaseView.summaryFailed", { n: r.failed }));
  if (r.errors.length > 0) parts.push(r.errors[0]);
  return parts.join(" · ");
}

const isUrlSource = (sourceId: string) => /^https?:\/\//i.test(sourceId);
const basename = (p: string) => p.split(/[\\/]/).pop() ?? p;

/** Module-level SWR snapshot: re-opening the KB page renders the last data
 * instantly while the refresh (several local requests plus cloud round
 * trips) runs in the background. Mutating actions already call load(), so
 * freshness after upload/delete/push/pull is preserved by those calls. View
 * state (panel, upload dialog, selection) is intentionally not cached. */
interface KbSnapshot {
  configured: boolean | null;
  reason: string | null;
  collections: CollectionSummary[];
  rows: DocRow[];
  cloudSt: CloudStatus | null;
  cloudKbs: CloudKb[];
  cloudFiles: Record<string, Set<string>>;
  cloudLoaded: boolean;
}

const KB_EMPTY_SNAPSHOT: KbSnapshot = {
  configured: null,
  reason: null,
  collections: [],
  rows: [],
  cloudSt: null,
  cloudKbs: [],
  cloudFiles: {},
  cloudLoaded: false,
};

let kbCache: KbSnapshot | null = null;
const cacheKb = (patch: Partial<KbSnapshot>) => {
  kbCache = { ...(kbCache ?? KB_EMPTY_SNAPSHOT), ...patch };
};

/** Per-document cloud backup state, derived from the cloud KB's file list. */
type BackupState =
  | { kind: "backed-up" }
  | { kind: "not-backed-up" }
  | { kind: "not-eligible"; why: string }
  | { kind: "unknown"; why: string };

/**
 * Full-screen Knowledge Base page (replaces the chat area when opened from
 * the sidebar; the title lives in the app Titlebar's `page` slot). One flat
 * document table across all per-agent collections (`kb-{agent_id}`, served
 * by the engine's embedded RAG and immediately retrievable by that agent):
 * each row shows its cloud backup state and can be pushed to Syscity Cloud
 * from where any device signed into the same account can restore it. The
 * cloud is storage only — indexing/retrieval stays local.
 */
export function KnowledgeBaseView({ agents }: { agents: KbAgent[] }) {
  const { t } = useTranslation("kb");
  const [configured, setConfigured] = useState<boolean | null>(kbCache?.configured ?? null);
  const [reason, setReason] = useState<string | null>(kbCache?.reason ?? null);
  const [collections, setCollections] = useState<CollectionSummary[]>(kbCache?.collections ?? []);
  const [rows, setRows] = useState<DocRow[]>(kbCache?.rows ?? []);
  const [loading, setLoading] = useState(!kbCache);
  const [error, setError] = useState<string | null>(null);
  const [busyDoc, setBusyDoc] = useState<string | null>(null);
  const [busyPush, setBusyPush] = useState<string | null>(null);
  const [pushNote, setPushNote] = useState<string | null>(null);

  // Cloud (graceful: the page works without it, backup cells show "—").
  const [cloudSt, setCloudSt] = useState<CloudStatus | null>(kbCache?.cloudSt ?? null);
  const [cloudKbs, setCloudKbs] = useState<CloudKb[]>(kbCache?.cloudKbs ?? []);
  const [cloudFiles, setCloudFiles] = useState<Record<string, Set<string>>>(
    kbCache?.cloudFiles ?? {}
  );
  // True once the per-KB file lists have loaded (or definitively failed) —
  // backup indicators stay hidden until then instead of flashing "not backed
  // up" while the check is still in flight.
  const [cloudLoaded, setCloudLoaded] = useState(kbCache?.cloudLoaded ?? false);
  const [pullAgent, setPullAgent] = useState<Record<string, string>>({});
  const [pullNotes, setPullNotes] = useState<Record<string, string>>({});
  const pollTimer = useRef<number | null>(null);

  // Cloud popover (opened from the toolbar status chip; click-away closes).
  const [cloudOpen, setCloudOpen] = useState(false);

  // Right-side document panel. Visibility is shared with the Titlebar's
  // "show right sidebar" button (kbPanelOpen); viewDoc holds which row it
  // shows. The pane is resizable on md+ with the same drag-divider pattern
  // as the chat split.
  const kbPanelOpen = useChatStore((s) => s.kbPanelOpen);
  const setKbPanelOpen = useChatStore((s) => s.setKbPanelOpen);
  const [paneRatio, setPaneRatio] = useState(0.42);
  const [paneDragging, setPaneDragging] = useState(false);
  const paneRowRef = useRef<HTMLDivElement | null>(null);

  // Upload dialog.
  const [uploadOpen, setUploadOpen] = useState(false);
  const [uploadAgent, setUploadAgent] = useState<string>(DEFAULT_AGENT);
  const [pendingFiles, setPendingFiles] = useState<File[]>([]);
  const [uploading, setUploading] = useState(false);
  const [uploadNote, setUploadNote] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  // Document viewer (click a row to preview its source file).
  const [viewDoc, setViewDoc] = useState<DocRow | null>(null);
  const [viewBody, setViewBody] = useState<
    | { kind: "loading" }
    | { kind: "text"; content: string; truncated: boolean }
    | { kind: "binary" }
    | { kind: "error"; message: string }
  >({ kind: "loading" });

  const openDoc = useCallback(async (row: DocRow) => {
    setViewDoc(row);
    setViewBody({ kind: "loading" });
    setKbPanelOpen(true);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("KnowledgeBaseView.noGateway"));
      const body = await transport.kbDocContent(row.collection, row.doc_id);
      if (body.binary) setViewBody({ kind: "binary" });
      else
        setViewBody({
          kind: "text",
          content: body.content ?? "",
          truncated: body.truncated,
        });
    } catch (e) {
      setViewBody({ kind: "error", message: e instanceof Error ? e.message : String(e) });
    }
  }, [setKbPanelOpen, t]);

  const cloudReady = !!cloudSt?.enabled && !!cloudSt.logged_in;

  // Local data only — the page can render as soon as this lands.
  const loadLocal = useCallback(async () => {
    const transport = getActiveTransport();
    if (!transport) throw new Error(t("KnowledgeBaseView.noGateway"));
    const body = await transport.listKbCollections();
    setConfigured(body.configured);
    setReason(body.reason);
    const cols = body.collections as unknown as CollectionSummary[];
    setCollections(cols);
    const results = await Promise.all(cols.map((c) => transport.listKbDocs(c.collection)));
    const nextRows = results.flatMap((r, i) =>
      (r.docs as unknown as KbDoc[]).map((d) => ({ ...d, collection: cols[i].collection }))
    );
    setRows(nextRows);
    cacheKb({
      configured: body.configured,
      reason: body.reason,
      collections: cols,
      rows: nextRows,
    });
  }, [t]);

  // Cloud state is best-effort and slower (network round trips to Syscity
  // Cloud): status failures degrade the backup column to "—"; list failures
  // keep the sign-in state but show no backups.
  const loadCloud = useCallback(async () => {
    let st: CloudStatus;
    try {
      st = await cloudStatus();
      setCloudSt(st);
      cacheKb({ cloudSt: st });
    } catch {
      setCloudSt(null);
      setCloudKbs([]);
      setCloudFiles({});
      setCloudLoaded(true);
      cacheKb({ cloudSt: null, cloudKbs: [], cloudFiles: {}, cloudLoaded: true });
      return;
    }
    if (!st.enabled || !st.logged_in) {
      setCloudKbs([]);
      setCloudFiles({});
      setCloudLoaded(true);
      cacheKb({ cloudKbs: [], cloudFiles: {}, cloudLoaded: true });
      return;
    }
    try {
      const transport = getActiveTransport();
      if (!transport) {
        setCloudLoaded(true);
        cacheKb({ cloudLoaded: true });
        return;
      }
      const kbBody = (await transport.cloudKbList()) as { knowledge_bases?: CloudKb[] } | undefined;
      const kbs = kbBody?.knowledge_bases ?? [];
      setCloudKbs(kbs);
      cacheKb({ cloudKbs: kbs });
      const docLists = await Promise.all(
        kbs.map((k) =>
          transport
            .cloudKbDocs(k.id)
            .then((d) => {
              const docs = ((d as { documents?: CloudKbDoc[] })?.documents ?? []).filter(
                (x) => x.status === "stored"
              );
              return [k.name, new Set(docs.map((x) => x.filename))] as const;
            })
            .catch(() => [k.name, new Set<string>()] as const)
        )
      );
      const filesMap = Object.fromEntries(docLists);
      setCloudFiles(filesMap);
      setCloudLoaded(true);
      cacheKb({ cloudFiles: filesMap, cloudLoaded: true });
    } catch {
      setCloudKbs([]);
      setCloudFiles({});
      setCloudLoaded(true);
      cacheKb({ cloudKbs: [], cloudFiles: {}, cloudLoaded: true });
    }
  }, []);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    let ok = true;
    try {
      await loadLocal();
    } catch (e) {
      ok = false;
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
    // Cloud refresh runs in the background — backup cells show "—" until it
    // lands instead of blocking the whole page behind a spinner.
    if (ok) void loadCloud();
  }, [loadLocal, loadCloud]);

  useEffect(() => {
    load();
    return () => {
      if (pollTimer.current) window.clearInterval(pollTimer.current);
      // The panel flag is per-visit: leaving the KB page closes the pane so
      // re-entering starts clean (the selected doc is component-local anyway).
      setKbPanelOpen(false);
    };
  }, [load, setKbPanelOpen]);

  const backupStateOf = (row: DocRow): BackupState => {
    if (!cloudSt?.enabled) return { kind: "unknown", why: "Cloud is not enabled" };
    if (!cloudSt.logged_in) return { kind: "unknown", why: "Sign in to Syscity Cloud to back up" };
    // "unknown" renders nothing — rows stay quiet until the cloud check lands
    // instead of flashing "not backed up" while it is still in flight.
    if (!cloudLoaded) return { kind: "unknown", why: "Checking cloud backups…" };
    if (isUrlSource(row.source_id))
      return { kind: "not-eligible", why: "URL sources have no bytes to back up" };
    const files = cloudFiles[row.collection];
    if (!files) return { kind: "not-backed-up" };
    return files.has(basename(row.source_id)) ? { kind: "backed-up" } : { kind: "not-backed-up" };
  };

  // ---- actions ------------------------------------------------------------

  const confirmUpload = async () => {
    if (pendingFiles.length === 0) return;
    setError(null);
    setUploadNote(null);
    setUploading(true);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("KnowledgeBaseView.noGateway"));
      const notes: string[] = [];
      for (const file of pendingFiles) {
        const ext = file.name.split(".").pop()?.toLowerCase() ?? "";
        if (!ALLOWED_EXT.includes(ext)) {
          notes.push(t("KnowledgeBaseView.unsupportedType", { name: file.name, ext }));
          continue;
        }
        if (file.size > MAX_UPLOAD_BYTES) {
          notes.push(
            t("KnowledgeBaseView.exceedsSize", {
              name: file.name,
              mb: MAX_UPLOAD_BYTES / (1024 * 1024),
            })
          );
          continue;
        }
        try {
          const base64 = await fileToBase64(file);
          await transport.ingestKbDoc(uploadAgent || DEFAULT_AGENT, file.name, base64);
        } catch (e) {
          notes.push(
            t("KnowledgeBaseView.uploadItemFailed", {
              name: file.name,
              error: e instanceof Error ? e.message : String(e),
            })
          );
        }
      }
      if (notes.length > 0) setUploadNote(notes.join(" · "));
      setPendingFiles([]);
      setUploadOpen(false);
      await load();
    } finally {
      setUploading(false);
      if (fileInputRef.current) fileInputRef.current.value = "";
    }
  };

  const push = async (collection: string) => {
    setBusyPush(collection);
    setPushNote(null);
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("KnowledgeBaseView.noGateway"));
      const r = (await transport.cloudKbPush(collection)) as PushResult;
      const summary = resultSummary(r, t("KnowledgeBaseView.pushed"), t);
      setPushNote(t("KnowledgeBaseView.pushNote", { collection, summary }));
      await load();
    } catch (e) {
      setPushNote(
        t("KnowledgeBaseView.pushFailed", {
          collection,
          error: e instanceof Error ? e.message : String(e),
        })
      );
    } finally {
      setBusyPush(null);
    }
  };

  const pushAll = async () => {
    setBusyPush("all");
    setPushNote(null);
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("KnowledgeBaseView.noGateway"));
      const parts: string[] = [];
      for (const c of collections) {
        try {
          const r = (await transport.cloudKbPush(c.collection)) as PushResult;
          if (r.pushed > 0 || r.failed > 0 || r.errors.length > 0)
            parts.push(
              t("KnowledgeBaseView.pushNote", {
                collection: c.collection,
                summary: resultSummary(r, t("KnowledgeBaseView.pushed"), t),
              })
            );
        } catch (e) {
          parts.push(
            t("KnowledgeBaseView.pushFailed", {
              collection: c.collection,
              error: e instanceof Error ? e.message : String(e),
            })
          );
        }
      }
      setPushNote(parts.length > 0 ? parts.join(" · ") : t("KnowledgeBaseView.allBackedUp"));
      await load();
    } finally {
      setBusyPush(null);
    }
  };

  const deleteDoc = async (row: DocRow) => {
    if (
      !confirm(
        t("KnowledgeBaseView.confirmDeleteDoc", { id: row.doc_id, collection: row.collection })
      )
    )
      return;
    setBusyDoc(`${row.collection}:${row.doc_id}`);
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("KnowledgeBaseView.noGateway"));
      await transport.deleteKbDoc(row.collection, row.doc_id);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyDoc(null);
    }
  };

  const signIn = () => {
    window.open(cloudLoginUrl("github"), "_blank");
    if (pollTimer.current) window.clearInterval(pollTimer.current);
    const started = Date.now();
    pollTimer.current = window.setInterval(async () => {
      try {
        const st = await cloudStatus();
        if (st.logged_in) {
          if (pollTimer.current) window.clearInterval(pollTimer.current);
          await load();
        }
      } catch {
        /* transient — keep polling */
      }
      if (Date.now() - started > LOGIN_TIMEOUT_MS && pollTimer.current) {
        window.clearInterval(pollTimer.current);
      }
    }, POLL_MS);
  };

  const pull = async (kb: CloudKb, agentId: string) => {
    if (!agentId) return;
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("KnowledgeBaseView.noGateway"));
      const r = (await transport.cloudKbPull({ cloud_kb_id: kb.id, agent_id: agentId })) as PullResult;
      setPullNotes((prev) => ({
        ...prev,
        [kb.id]: resultSummary(r, t("KnowledgeBaseView.restored"), t),
      }));
      await load();
    } catch (e) {
      setPullNotes((prev) => ({
        ...prev,
        [kb.id]: e instanceof Error ? e.message : String(e),
      }));
    }
  };

  const deleteKb = async (kb: CloudKb) => {
    if (!confirm(t("KnowledgeBaseView.confirmDeleteCloudKb", { name: kb.name }))) return;
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("KnowledgeBaseView.noGateway"));
      await transport.cloudKbDelete(kb.id);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  // ---- render ---------------------------------------------------------------

  // No full-page spinner: the shell (toolbar + list) renders immediately and
  // local data pops in as it lands; the cloud refresh is already backgrounded.

  // Default install state: no embedding provider configured. Guide the user
  // instead of showing an empty manager.
  if (configured === false) {
    return (
      <div className="flex-1 overflow-y-auto bg-page">
        <div className="max-w-lg mx-auto mt-10 text-center">
          <FileText className="w-10 h-10 mx-auto mb-3 text-secondary/60" />
          <h3 className="text-sm font-semibold text-primary mb-1">
            {t("KnowledgeBaseView.notConfigured")}
          </h3>
          <p className="text-xs text-secondary mb-3">
            {reason ?? t("KnowledgeBaseView.noEmbeddingProvider")}
          </p>
          <div className="text-xs text-secondary bg-card rounded-lg p-3 text-left">
            <p className="mb-1">
              {t("KnowledgeBaseView.inConfig")}{" "}
              <code className="px-1 rounded bg-black/5 dark:bg-white/10">~/.syscity/config.toml</code>
              {t("KnowledgeBaseView.configColon")}
            </p>
            <pre className="text-[11px] whitespace-pre-wrap">{`[vector_memory]
provider = "open_ai"
embedding_api_key = "sk-..."`}</pre>
          </div>
        </div>
      </div>
    );
  }

  const totalChunks = collections.reduce((n, c) => n + c.total_chunks, 0);
  const backedUpCount = rows.filter((r) => backupStateOf(r).kind === "backed-up").length;

  return (
    <div ref={paneRowRef} className="flex-1 flex overflow-hidden bg-page">
      {/* Left column: toolbar + document list. The right side hosts the
          document preview pane (md+ split with drag divider; full-screen
          overlay below md). */}
      <div className="flex-1 min-w-0 flex flex-col overflow-hidden">
      {/* Toolbar — Upload is the only primary action; the cloud lives behind
          a quiet status chip on the right (popover, click-away closes). */}
      <div className="flex items-center gap-2 px-6 md:px-8 py-3 shrink-0">
        <button
          onClick={() => {
            setUploadAgent(DEFAULT_AGENT);
            setPendingFiles([]);
            setUploadNote(null);
            setUploadOpen(true);
          }}
          className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium bg-primary-600 text-white hover:bg-primary-700 transition"
        >
          <Upload className="w-3.5 h-3.5" />
          {t("KnowledgeBaseView.upload")}
        </button>
        <div className="flex-1" />
        <span className="text-[11px] text-secondary">
          {t("KnowledgeBaseView.stats", {
            docs: rows.length,
            collections: collections.length,
            chunks: totalChunks,
          })}
        </span>
        {cloudSt?.enabled && (
          <div className="relative">
            <button
              onClick={() => setCloudOpen((v) => !v)}
              title={
                cloudReady
                  ? t("KnowledgeBaseView.cloudBackups")
                  : t("KnowledgeBaseView.cloudSignInTitle")
              }
              className={`inline-flex items-center gap-1 px-2 py-1.5 rounded-md text-[11px] font-medium border transition ${
                cloudReady && cloudLoaded && rows.length > 0 && backedUpCount === rows.length
                  ? "border-green-500/30 bg-green-50 dark:bg-green-900/20 text-green-700 dark:text-green-400"
                  : "border-subtle bg-card text-secondary hover:text-primary hover:bg-black/5 dark:hover:bg-white/5"
              }`}
            >
              <Cloud className="w-3.5 h-3.5" />
              {cloudReady && cloudLoaded && rows.length > 0 && (
                <span>
                  {backedUpCount}/{rows.length}
                </span>
              )}
            </button>
            {cloudOpen && (
              <>
                <div className="fixed inset-0 z-40" onClick={() => setCloudOpen(false)} />
                <div className="absolute right-0 top-full mt-2 z-50 w-[24rem] max-w-[92vw] bg-card rounded-xl shadow-xl border border-subtle overflow-hidden">
                  <div className="flex items-center justify-between px-4 py-2.5 border-b border-subtle">
                    <p className="text-xs font-semibold text-primary">
                      {t("KnowledgeBaseView.cloudBackups")}
                    </p>
                    {cloudReady && cloudSt?.user && (
                      <p className="text-[10px] text-secondary truncate max-w-[12rem]">
                        {cloudSt.user.name || cloudSt.user.email}
                      </p>
                    )}
                  </div>
                  {!cloudReady ? (
                    <div className="px-4 py-4 space-y-3">
                      <p className="text-xs text-secondary">
                        {t("KnowledgeBaseView.cloudPopoverBody")}
                      </p>
                      <button
                        onClick={signIn}
                        className="w-full inline-flex items-center justify-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium bg-primary-600 text-white hover:bg-primary-700 transition"
                      >
                        <Cloud className="w-3.5 h-3.5" />
                        {t("KnowledgeBaseView.signIn")}
                      </button>
                    </div>
                  ) : (
                    <>
                      <div className="max-h-[18rem] overflow-y-auto divide-y divide-subtle">
                        {cloudKbs.length === 0 ? (
                          <p className="px-4 py-6 text-center text-xs text-secondary">
                            {t("KnowledgeBaseView.noBackupsYet")}
                          </p>
                        ) : (
                          cloudKbs.map((kb) => {
                            // One-click restore when the backup name maps to a local agent.
                            const mappedAgent = kb.name.startsWith("kb-")
                              ? agents.find((a) => a.id === kb.name.slice(3))
                              : undefined;
                            const chosenAgent = pullAgent[kb.id] ?? "";
                            return (
                              <div key={kb.id} className="px-4 py-2.5 space-y-2">
                                <div className="flex items-center gap-2">
                                  <Cloud className="w-3.5 h-3.5 shrink-0 text-secondary" />
                                  <p className="text-sm text-primary truncate flex-1">{kb.name}</p>
                                  <span className="text-[10px] text-secondary/70 shrink-0">
                                    {t("KnowledgeBaseView.docsCount", { n: kb.document_count ?? 0 })}
                                  </span>
                                  <button
                                    onClick={() => deleteKb(kb)}
                                    className="p-1 rounded-md text-secondary hover:text-red-500 hover:bg-red-500/10 transition shrink-0"
                                    title={t("KnowledgeBaseView.deleteCloudBackup")}
                                    aria-label={t("KnowledgeBaseView.deleteBackupNamed", {
                                      name: kb.name,
                                    })}
                                  >
                                    <Trash2 className="w-3.5 h-3.5" />
                                  </button>
                                </div>
                                {pullNotes[kb.id] && (
                                  <p className="text-[10px] text-secondary">{pullNotes[kb.id]}</p>
                                )}
                                <div className="flex items-center gap-2">
                                  {mappedAgent ? (
                                    <button
                                      onClick={() => pull(kb, mappedAgent.id)}
                                      className="inline-flex items-center gap-1 px-2.5 py-1.5 rounded-md text-xs font-medium bg-primary-600 text-white hover:bg-primary-700 transition"
                                      title={t("KnowledgeBaseView.restoreIntoCollection", {
                                        name: mappedAgent.display_name,
                                      })}
                                    >
                                      {t("KnowledgeBaseView.restoreTo", {
                                        name: mappedAgent.display_name,
                                      })}
                                    </button>
                                  ) : (
                                    <>
                                      <select
                                        value={chosenAgent}
                                        onChange={(e) =>
                                          setPullAgent((prev) => ({
                                            ...prev,
                                            [kb.id]: e.target.value,
                                          }))
                                        }
                                        className="text-xs px-2 py-1.5 rounded-md bg-page text-primary border border-subtle focus:outline-none focus:ring-2 focus:ring-primary-500/20 max-w-36"
                                      >
                                        <option value="">
                                          {t("KnowledgeBaseView.restoreToPlaceholder")}
                                        </option>
                                        <option value={DEFAULT_AGENT}>
                                          {t("KnowledgeBaseView.defaultAgent")}
                                        </option>
                                        {agents
                                          .filter((a) => a.id !== DEFAULT_AGENT)
                                          .map((a) => (
                                            <option key={a.id} value={a.id}>
                                              {a.emoji} {a.display_name}
                                            </option>
                                          ))}
                                      </select>
                                      <button
                                        onClick={() => pull(kb, chosenAgent)}
                                        disabled={!chosenAgent}
                                        className="inline-flex items-center gap-1 px-2.5 py-1.5 rounded-md text-xs font-medium bg-primary-600 text-white hover:bg-primary-700 transition disabled:opacity-50"
                                      >
                                        {t("KnowledgeBaseView.restore")}
                                      </button>
                                    </>
                                  )}
                                </div>
                              </div>
                            );
                          })
                        )}
                      </div>
                      {collections.length > 0 && (
                        <div className="px-3 py-2.5 border-t border-subtle">
                          <button
                            onClick={pushAll}
                            disabled={busyPush !== null}
                            className="w-full inline-flex items-center justify-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium text-primary border border-subtle hover:bg-black/5 dark:hover:bg-white/5 transition disabled:opacity-50"
                          >
                            {busyPush === "all" ? (
                              <Loader2 className="w-3.5 h-3.5 animate-spin" />
                            ) : (
                              <CloudUpload className="w-3.5 h-3.5" />
                            )}
                            {t("KnowledgeBaseView.backUpAll")}
                          </button>
                        </div>
                      )}
                    </>
                  )}
                </div>
              </>
            )}
          </div>
        )}
        <button
          onClick={load}
          className="p-1 rounded-md text-secondary hover:text-primary hover:bg-black/5 dark:hover:bg-white/5 transition"
          title={t("KnowledgeBaseView.refresh")}
          aria-label={t("KnowledgeBaseView.refresh")}
        >
          <RefreshCw className="w-3.5 h-3.5" />
        </button>
      </div>

      <div className="flex-1 overflow-y-auto px-6 md:px-8 py-4 space-y-6">
        {error && <div className="text-xs text-red-600 dark:text-red-400">{error}</div>}
        {pushNote && <div className="text-xs text-secondary">{pushNote}</div>}
        {uploadNote && <div className="text-xs text-amber-600 dark:text-amber-400">{uploadNote}</div>}

        {/* All documents across collections */}
        <div className="rounded-lg bg-card divide-y divide-subtle">
          {rows.length === 0 ? (
            <div className="py-10 text-center text-xs text-secondary">
              {loading ? (
                <span className="inline-flex items-center gap-2">
                  <Loader2 className="w-3.5 h-3.5 animate-spin" />{" "}
                  {t("KnowledgeBaseView.loadingCollections")}
                </span>
              ) : (
                t("KnowledgeBaseView.noDocuments")
              )}
            </div>
          ) : (
            rows.map((d) => {
              const owner = ownerOf(d.collection, agents, t);
              const bs = backupStateOf(d);
              return (
                <div
                  key={`${d.collection}:${d.doc_id}`}
                  className="flex items-center gap-3 px-4 py-2.5 cursor-pointer hover:bg-black/[0.03] dark:hover:bg-white/[0.03] transition"
                  onClick={() => openDoc(d)}
                  title={t("KnowledgeBaseView.viewDocument")}
                >
                  <FileText className="w-4 h-4 shrink-0 text-secondary" />
                  <div className="min-w-0 flex-1">
                    <p className="text-sm text-primary truncate" title={d.source_id}>
                      {d.doc_id}
                    </p>
                    <p className="text-[10px] text-secondary/70 truncate">
                      {t("KnowledgeBaseView.docMeta", {
                        chunks: d.chunk_count,
                        date: new Date(d.indexed_at).toLocaleString(),
                      })}
                      {d.error ? ` · ${d.error}` : ""}
                    </p>
                  </div>
                  {d.collection !== "kb-default" && (
                    <span
                      className="hidden sm:inline-flex items-center gap-1 w-28 justify-center px-2 py-0.5 rounded-full bg-black/5 dark:bg-white/10 text-[10px] text-secondary shrink-0"
                      title={t("KnowledgeBaseView.collectionNamed", { name: d.collection })}
                    >
                      <span aria-hidden="true">{owner.emoji}</span>
                      <span className="truncate">{owner.label}</span>
                    </span>
                  )}
                  <span
                    className={`w-20 justify-center px-2 py-0.5 rounded-full text-[10px] font-medium text-center shrink-0 ${statusBadge(d.status)}`}
                  >
                    {d.status}
                  </span>
                  {/* Cloud backup state: quiet micro-icon; invisible when the
                      cloud is off, not signed in, or the source isn't
                      eligible (URL) — the chip popover handles the rest. */}
                  {bs.kind === "backed-up" && (
                    <span
                      className="shrink-0 flex justify-center w-6"
                      title={t("KnowledgeBaseView.backedUp")}
                    >
                      <Cloud className="w-3.5 h-3.5 text-green-600 dark:text-green-400" />
                    </span>
                  )}
                  {bs.kind === "not-backed-up" && (
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        push(d.collection);
                      }}
                      disabled={busyPush !== null}
                      className="p-1.5 rounded-md text-secondary hover:text-primary-600 dark:hover:text-primary-400 hover:bg-primary-50 dark:hover:bg-primary-900/20 transition disabled:opacity-50 shrink-0"
                      title={t("KnowledgeBaseView.backUpCollection", { name: d.collection })}
                    >
                      {busyPush === d.collection ? (
                        <Loader2 className="w-3.5 h-3.5 animate-spin" />
                      ) : (
                        <CloudUpload className="w-3.5 h-3.5" />
                      )}
                    </button>
                  )}
                  <button
                    onClick={(e) => {
                      e.stopPropagation();
                      deleteDoc(d);
                    }}
                    disabled={busyDoc === `${d.collection}:${d.doc_id}`}
                    className="p-1.5 rounded-md text-secondary hover:text-red-500 hover:bg-red-500/10 transition disabled:opacity-50 shrink-0"
                    title={t("KnowledgeBaseView.deleteDocument")}
                    aria-label={t("KnowledgeBaseView.deleteNamed", { name: d.doc_id })}
                  >
                    {busyDoc === `${d.collection}:${d.doc_id}` ? (
                      <Loader2 className="w-3.5 h-3.5 animate-spin" />
                    ) : (
                      <Trash2 className="w-3.5 h-3.5" />
                    )}
                  </button>
                </div>
              );
            })
          )}
        </div>
      </div>

      </div>

      {/* Drag divider (md+): same pattern as the chat right-pane split. */}
      {kbPanelOpen && (
        <div
          className={`relative hidden md:block w-px shrink-0 cursor-col-resize after:absolute after:inset-y-0 after:-left-1.5 after:-right-1.5 after:content-[''] transition-colors ${
            paneDragging ? "bg-primary-500" : "bg-black/15 dark:bg-white/25 hover:bg-primary-400/60"
          }`}
          onMouseDown={(e) => {
            e.preventDefault();
            const row = paneRowRef.current;
            if (!row) return;
            const rect = row.getBoundingClientRect();
            setPaneDragging(true);
            document.body.style.userSelect = "none";
            document.body.style.cursor = "col-resize";

            const onMove = (me: MouseEvent) => {
              const x = Math.max(0, Math.min(rect.width, me.clientX - rect.left));
              const ratio = x / rect.width;
              setPaneRatio(1 - Math.max(0.2, Math.min(0.8, ratio)));
            };
            const onUp = () => {
              setPaneDragging(false);
              document.body.style.userSelect = "";
              document.body.style.cursor = "";
              window.removeEventListener("mousemove", onMove);
              window.removeEventListener("mouseup", onUp);
            };
            window.addEventListener("mousemove", onMove);
            window.addEventListener("mouseup", onUp);
          }}
        />
      )}

      {/* Right pane: document preview (text/markdown only — binary formats
          and URL sources show an explanatory note). Below md it takes the
          whole screen as an overlay; on md+ it is a resizable split column
          whose width comes from --pane-w. Without a selected document the
          pane shows a hint (md+ only, so mobile never gets an empty
          full-screen aside). */}
      {kbPanelOpen && (
        <aside
          className={`shrink-0 flex-col ${
            viewDoc
              ? "fixed inset-0 z-30 bg-page flex md:static md:z-auto md:flex-none md:w-[var(--pane-w)]"
              : "hidden md:flex md:flex-none md:w-[var(--pane-w)] bg-page"
          }`}
          style={{ "--pane-w": `${paneRatio * 100}%` } as CSSProperties}
        >
          {viewDoc ? (
            <>
              <div className="flex items-center gap-2 px-4 py-3 shrink-0">
                <FileText className="w-4 h-4 shrink-0 text-secondary" />
                <div className="min-w-0 flex-1">
                  <p className="text-sm font-medium text-primary truncate">{viewDoc.doc_id}</p>
                  <p className="text-[10px] text-secondary/70 truncate">
                    {viewDoc.collection}
                    {viewBody.kind === "text" &&
                      ` · ${
                        viewBody.truncated
                          ? t("KnowledgeBaseView.previewTruncated")
                          : t("KnowledgeBaseView.previewFull")
                      }`}
                  </p>
                </div>
                <button
                  onClick={() => {
                    setViewDoc(null);
                    setKbPanelOpen(false);
                  }}
                  className="p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition shrink-0"
                  title={t("KnowledgeBaseView.close")}
                  aria-label={t("KnowledgeBaseView.closeViewer")}
                >
                  <X className="w-4 h-4" />
                </button>
              </div>
              <div className="flex-1 overflow-y-auto px-5 py-4">
                {viewBody.kind === "loading" && (
                  <div className="flex items-center justify-center gap-2 text-secondary text-sm py-10">
                    <Loader2 className="w-4 h-4 animate-spin" /> {t("KnowledgeBaseView.loading")}
                  </div>
                )}
                {viewBody.kind === "error" && (
                  <p className="text-xs text-red-600 dark:text-red-400">{viewBody.message}</p>
                )}
                {viewBody.kind === "binary" && (
                  <p className="text-xs text-secondary py-10 text-center">
                    {t("KnowledgeBaseView.binaryNote")}
                  </p>
                )}
                {viewBody.kind === "text" &&
                  (/\.md$/i.test(basename(viewDoc.source_id)) ? (
                    <MarkdownMessage text={viewBody.content} />
                  ) : (
                    <pre className="text-xs font-mono whitespace-pre-wrap text-primary leading-relaxed">
                      {viewBody.content}
                    </pre>
                  ))}
              </div>
            </>
          ) : (
            <div className="flex-1 flex items-center justify-center px-6">
              <p className="text-xs text-secondary text-center">
                {t("KnowledgeBaseView.selectHint")}
              </p>
            </div>
          )}
        </aside>
      )}

      {/* Upload dialog: pick files, optionally pick the destination agent
          (defaults to the default agent's shared collection). */}
      {uploadOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center">
          <div className="absolute inset-0 bg-black/40" onClick={() => !uploading && setUploadOpen(false)} />
          <div className="relative bg-card rounded-xl shadow-xl p-5 w-[26rem] max-w-[90vw] space-y-4">
            <h3 className="text-sm font-semibold text-primary">
              {t("KnowledgeBaseView.uploadDocuments")}
            </h3>
            <label className="block">
              <span className="block text-xs text-secondary mb-1">
                {t("KnowledgeBaseView.destinationAgent")}
              </span>
              <select
                value={uploadAgent}
                onChange={(e) => setUploadAgent(e.target.value)}
                className="w-full text-sm px-3 py-2 rounded-md bg-page text-primary border border-subtle focus:outline-none focus:ring-2 focus:ring-primary-500/20"
              >
                <option value={DEFAULT_AGENT}>{t("KnowledgeBaseView.defaultAgent")}</option>
                {agents
                  .filter((a) => a.id !== DEFAULT_AGENT)
                  .map((a) => (
                    <option key={a.id} value={a.id}>
                      {a.emoji} {a.display_name} ({a.id})
                    </option>
                  ))}
              </select>
            </label>
            <p className="text-[10px] text-secondary/70">
              {t("KnowledgeBaseView.uploadHintPrefix")}{" "}
              <code className="px-1 rounded bg-black/5 dark:bg-white/10">
                kb-{uploadAgent || DEFAULT_AGENT}
              </code>{" "}
              {t("KnowledgeBaseView.uploadHintMiddle")}{" "}
              {ALLOWED_EXT.map((e) => `.${e}`).join(" ")}
              {t("KnowledgeBaseView.uploadHintSize", { mb: MAX_UPLOAD_BYTES / (1024 * 1024) })}
            </p>
            <input
              ref={fileInputRef}
              type="file"
              multiple
              accept={ALLOWED_EXT.map((e) => `.${e}`).join(",")}
              onChange={(e) => setPendingFiles(Array.from(e.target.files ?? []))}
              className="w-full text-sm text-secondary file:mr-3 file:py-2 file:px-3 file:rounded-md file:border-0 file:text-xs file:font-medium file:bg-primary-50 file:text-primary-700 dark:file:bg-primary-900/20 dark:file:text-primary-400 hover:file:bg-primary-100"
            />
            {pendingFiles.length > 0 && (
              <p className="text-[11px] text-secondary truncate">
                {pendingFiles.map((f) => f.name).join(", ")}
              </p>
            )}
            <div className="flex justify-end gap-2">
              <button
                onClick={() => setUploadOpen(false)}
                disabled={uploading}
                className="px-3 py-1.5 rounded-md text-xs font-medium text-secondary hover:bg-black/5 dark:hover:bg-white/5 transition disabled:opacity-50"
              >
                {t("KnowledgeBaseView.cancel")}
              </button>
              <button
                onClick={confirmUpload}
                disabled={uploading || pendingFiles.length === 0}
                className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium bg-primary-600 text-white hover:bg-primary-700 transition disabled:opacity-50"
              >
                {uploading && <Loader2 className="w-3.5 h-3.5 animate-spin" />}
                {t("KnowledgeBaseView.upload")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
