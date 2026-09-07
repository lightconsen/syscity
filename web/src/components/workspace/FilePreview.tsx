import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertCircle, ArrowLeft, Download, Loader2 } from "lucide-react";
import { MarkdownMessage } from "@/components/shared/MarkdownMessage";
import { CodeBlock } from "@/components/shared/CodeBlock";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";

/** Extension → shiki language id (CodeBlock falls back to plain text on unknown). */
const LANG_BY_EXT: Record<string, string> = {
  ts: "typescript",
  tsx: "tsx",
  js: "javascript",
  jsx: "jsx",
  json: "json",
  rs: "rust",
  py: "python",
  toml: "toml",
  yaml: "yaml",
  yml: "yaml",
  sh: "bash",
  css: "css",
  html: "html",
  sql: "sql",
  go: "go",
  java: "java",
  c: "c",
  cpp: "cpp",
  h: "c",
  swift: "swift",
  kt: "kotlin",
};

function extOf(path: string): string {
  const base = path.split("/").pop() ?? "";
  const idx = base.lastIndexOf(".");
  return idx > 0 ? base.slice(idx + 1).toLowerCase() : "";
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

interface FilePreviewProps {
  transport: SyscityWebSocketTransport;
  agentId?: string;
  path: string;
  onBack: () => void;
}

type LoadState =
  | { status: "loading" }
  | { status: "error"; message: string }
  | {
      status: "loaded";
      content?: string;
      size: number;
      truncated: boolean;
      binary: boolean;
    };

/** Inline preview of a single workspace file (markdown / code / plain text). */
export function FilePreview({ transport, agentId, path, onBack }: FilePreviewProps) {
  const { t } = useTranslation("workspace");
  const [state, setState] = useState<LoadState>({ status: "loading" });

  useEffect(() => {
    let cancelled = false;
    setState({ status: "loading" });
    transport
      .workspaceRead(agentId, path)
      .then((res) => {
        if (cancelled) return;
        setState({
          status: "loaded",
          content: res.content,
          size: res.size,
          truncated: res.truncated,
          binary: res.binary,
        });
      })
      .catch((err) => {
        if (cancelled) return;
        setState({
          status: "error",
          message: err instanceof Error ? err.message : t("FilePreview.failedToLoad"),
        });
      });
    return () => {
      cancelled = true;
    };
  }, [transport, agentId, path, t]);

  const handleDownload = useCallback(() => {
    if (state.status !== "loaded" || !state.content) return;
    const blob = new Blob([state.content], { type: "text/plain" });
    const objUrl = URL.createObjectURL(blob);
    const a = window.document.createElement("a");
    a.href = objUrl;
    a.download = path.split("/").pop() ?? "file";
    a.click();
    URL.revokeObjectURL(objUrl);
  }, [state, path]);

  const ext = extOf(path);
  const fileName = path.split("/").pop() ?? path;

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      {/* Preview toolbar */}
      <div className="shrink-0 flex items-center gap-2 px-3 py-2 border-b border-subtle">
        <button
          type="button"
          onClick={onBack}
          className="p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition"
          title={t("FilePreview.backToTree")}
          aria-label={t("FilePreview.backToTree")}
        >
          <ArrowLeft className="w-4 h-4" />
        </button>
        <span className="text-xs text-secondary truncate flex-1" title={path}>
          {path}
        </span>
        <button
          type="button"
          onClick={handleDownload}
          disabled={state.status !== "loaded" || !state.content}
          className="p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition disabled:opacity-30 disabled:pointer-events-none"
          title={t("FilePreview.downloadFile")}
          aria-label={t("FilePreview.downloadFile")}
        >
          <Download className="w-4 h-4" />
        </button>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto p-4">
        {state.status === "loading" && (
          <div className="flex items-center justify-center h-40 text-secondary">
            <Loader2 className="w-6 h-6 animate-spin" />
            <span className="ml-2 text-sm">{t("FilePreview.loading")}</span>
          </div>
        )}
        {state.status === "error" && (
          <div className="flex flex-col items-center justify-center h-40 text-secondary gap-2">
            <AlertCircle className="w-8 h-8 text-red-400" />
            <p className="text-sm">{state.message}</p>
            <p className="text-xs opacity-60">{path}</p>
          </div>
        )}
        {state.status === "loaded" && state.binary && (
          <div className="flex flex-col items-center justify-center h-40 text-secondary gap-2">
            <AlertCircle className="w-8 h-8 opacity-60" />
            <p className="text-sm">{t("FilePreview.binary")}</p>
            <p className="text-xs opacity-60">
              {fileName} · {formatSize(state.size)}
            </p>
          </div>
        )}
        {state.status === "loaded" && !state.binary && state.content !== undefined && (
          <>
            {state.truncated && (
              <div className="mb-3 px-3 py-2 rounded-md bg-amber-500/10 text-amber-600 dark:text-amber-400 text-xs">
                {t("FilePreview.largeFile", { size: formatSize(state.size) })}
              </div>
            )}
            {ext === "md" || ext === "markdown" ? (
              <div className="document-preview-content">
                <MarkdownMessage text={state.content} />
              </div>
            ) : LANG_BY_EXT[ext] ? (
              <CodeBlock code={state.content} language={LANG_BY_EXT[ext]} />
            ) : (
              <pre className="rounded-xl bg-sidebar p-4 overflow-x-auto text-xs font-mono leading-relaxed text-primary whitespace-pre-wrap break-words">
                {state.content}
              </pre>
            )}
          </>
        )}
      </div>
    </div>
  );
}
