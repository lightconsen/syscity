import { useState, useEffect, useCallback, useRef } from "react";
import { useTranslation } from "react-i18next";
import {
  X,
  FileText,
  Loader2,
  AlertCircle,
  FolderOpen,
  Share2,
  Download,
  Check,
} from "lucide-react";
import { MarkdownMessage } from "./MarkdownMessage";
import { apiFetch, getGatewayBase } from "@/lib/gatewayBase";

interface DocumentPreviewPanelProps {
  document: {
    filename: string;
    title: string;
    format: string;
    url?: string;
    exportUrl?: string;
  };
  onClose: () => void;
}

type LoadState = "loading" | "loaded" | "error";

/** Baseline table styling injected into the shadow DOM so authored table
 *  HTML (docx/xlsx) renders as a real bordered spreadsheet instead of a
 *  borderless run of text. Inline styles on the elements take precedence. */
const TABLE_STYLES = `
<style>
  table { border-collapse: collapse; width: 100%; margin: 0.5em 0; font-size: 13px; }
  th, td { border: 1px solid #e2e8f0; padding: 6px 10px; text-align: left; vertical-align: top; }
  th { background: #f1f5f9; font-weight: 600; color: #1f2937; }
  tbody tr:nth-child(even) { background: #f8fafc; }
</style>`;

/** Render HTML content inside a Shadow DOM root — no iframe event isolation issues. */
function HtmlShadowDom({ content }: { content: string }) {
  const hostRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    let root = host.shadowRoot;
    if (!root) {
      root = host.attachShadow({ mode: "open" });
    }
    root.innerHTML = content + TABLE_STYLES;
  }, [content]);

  return <div ref={hostRef} className="w-full h-full overflow-y-auto" />;
}

/** Map an authored-format name to its Office export file extension. */
function exportExtension(format: string): string {
  if (format === "slides") return "pptx";
  if (format === "docx") return "docx";
  if (format === "xlsx") return "xlsx";
  return "pptx";
}

/** Slides canvas preview: renders the canvas HTML, scaling each 1280px-wide
 *  `.slide` to the panel width (zoom keeps layout metrics honest, unlike
 *  transform: scale which leaves the unscaled box in flow). */
function SlidesShadowDom({ content }: { content: string }) {
  const hostRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    let root = host.shadowRoot;
    if (!root) {
      root = host.attachShadow({ mode: "open" });
    }
    root.innerHTML =
      content +
      `<style>
        body { margin: 0; }
        .slide { margin: 0 auto 16px; box-shadow: 0 4px 24px rgba(0,0,0,.12);
                 border-radius: 6px; overflow: hidden; }
      </style>`;
    const applyZoom = () => {
      const scale = host.clientWidth / 1280;
      root.querySelectorAll<HTMLElement>(".slide").forEach((s) => {
        s.style.zoom = String(scale);
      });
    };
    applyZoom();
    const ro = new ResizeObserver(applyZoom);
    ro.observe(host);
    return () => ro.disconnect();
  }, [content]);

  return <div ref={hostRef} className="w-full h-full overflow-y-auto p-2" />;
}

export function DocumentPreviewPanel({
  document,
  onClose,
}: DocumentPreviewPanelProps) {
  const [content, setContent] = useState<string | null>(null);
  const [loadState, setLoadState] = useState<LoadState>("loading");

  useEffect(() => {
    let cancelled = false;
    setLoadState("loading");
    setContent(null);

    // Prefer the owner-addressed URL from the tool result; fall back to the
    // legacy flat path for older artifacts. `apiFetch` prepends the gateway
    // base for relative paths and attaches the gateway token (mobile/remote).
    const artifactUrl =
      document.url ?? `/api/v1/artifacts/${document.filename}`;
    apiFetch(artifactUrl)
      .then((res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        return res.text();
      })
      .then((text) => {
        if (cancelled) return;
        setContent(text);
        setLoadState("loaded");
      })
      .catch((err) => {
        if (cancelled) return;
        console.error("Failed to load document:", err);
        setLoadState("error");
      });

    return () => {
      cancelled = true;
    };
  }, [document.filename]);

  const { t } = useTranslation("common");
  const [copied, setCopied] = useState(false);
  const isTauri = typeof window !== "undefined" && "__TAURI__" in window;

  const handleOpenFolder = useCallback(async () => {
    if (!isTauri) return;
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("reveal_in_folder", { filename: document.filename });
    } catch (err) {
      console.error("Failed to open folder:", err);
    }
  }, [document.filename, isTauri]);

  const handleCopyLink = useCallback(async () => {
    const url = `${getGatewayBase()}/api/v1/artifacts/${document.filename}`;
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Fallback for environments without clipboard API
      const ta = window.document.createElement("textarea");
      ta.value = url;
      window.document.body.appendChild(ta);
      ta.select();
      window.document.execCommand("copy");
      window.document.body.removeChild(ta);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }
  }, [document.filename]);

  const handleDownload = useCallback(async () => {
    // Server-side export (authored HTML → real Office file) when available.
    if (document.exportUrl) {
      try {
        const res = await apiFetch(document.exportUrl);
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const blob = await res.blob();
        const objUrl = URL.createObjectURL(blob);
        const a = window.document.createElement("a");
        a.href = objUrl;
        const ext = exportExtension(document.format);
        a.download = document.filename.replace(/\.(html?|md)$/i, "") + "." + ext;
        a.click();
        URL.revokeObjectURL(objUrl);
      } catch (err) {
        console.error("Failed to export document:", err);
      }
      return;
    }
    if (!content) return;
    const blob = new Blob([content], {
      type: document.format === "html" ? "text/html" : "text/markdown",
    });
    const objUrl = URL.createObjectURL(blob);
    const a = window.document.createElement("a");
    a.href = objUrl;
    a.download = document.filename;
    a.click();
    URL.revokeObjectURL(objUrl);
  }, [content, document.filename, document.format, document.exportUrl]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      onClose();
    }
  };

  // Focus trap: auto-focus the panel on mount for ESC key handling
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  return (
    <div
      className="flex-1 min-w-0 border-l border-subtle bg-page flex flex-col overflow-hidden"
      role="complementary"
      aria-label={t("DocumentPreviewPanel.ariaLabel")}
      onKeyDown={handleKeyDown}
    >
      {/* Title bar */}
      <div className="shrink-0 h-14 flex items-center justify-between px-4 border-b border-subtle">
        <div className="flex items-center gap-2 min-w-0">
          <FileText className="w-5 h-5 text-primary-500 shrink-0" />
          <div className="min-w-0">
            <div className="text-sm font-medium text-primary truncate">
              {document.title}
            </div>
            <div className="text-[10px] text-secondary">
              <span className="inline-flex items-center px-1 py-0.5 rounded text-[9px] font-medium bg-black/5 dark:bg-white/10">
                {document.format === "slides"
                  ? "PPT"
                  : document.format === "docx"
                    ? "DOCX"
                    : document.format === "xlsx"
                      ? "XLSX"
                      : document.format === "html"
                        ? "HTML"
                        : "MD"}
              </span>
            </div>
          </div>
        </div>
        <div className="flex items-center gap-1">
          {/* Open in folder — Tauri desktop only */}
          {isTauri && (
            <button
              type="button"
              onClick={handleOpenFolder}
              className="p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition"
              title={t("DocumentPreviewPanel.openFileLocation")}
              aria-label={t("DocumentPreviewPanel.openFileLocation")}
            >
              <FolderOpen className="w-4 h-4" />
            </button>
          )}
          {/* Download (Office export when the document has an exportUrl) */}
          <button
            type="button"
            onClick={handleDownload}
            disabled={loadState !== "loaded"}
            className="p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition disabled:opacity-30 disabled:pointer-events-none"
            title={document.exportUrl ? t("DocumentPreviewPanel.downloadAs", { ext: exportExtension(document.format).toUpperCase() }) : t("DocumentPreviewPanel.downloadFile")}
            aria-label={document.exportUrl ? t("DocumentPreviewPanel.downloadAs", { ext: exportExtension(document.format).toUpperCase() }) : t("DocumentPreviewPanel.downloadFile")}
          >
            <Download className="w-4 h-4" />
          </button>
          {/* Copy link / Share */}
          <button
            type="button"
            onClick={handleCopyLink}
            className="p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition"
            title={copied ? t("DocumentPreviewPanel.copied") : t("DocumentPreviewPanel.copyLink")}
            aria-label={t("DocumentPreviewPanel.copyLink")}
          >
            {copied ? (
              <Check className="w-4 h-4 text-green-500" />
            ) : (
              <Share2 className="w-4 h-4" />
            )}
          </button>
          {/* Close */}
          <button
            type="button"
            onClick={onClose}
            className="p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition"
            title={t("DocumentPreviewPanel.closePreviewEsc")}
            aria-label={t("DocumentPreviewPanel.closePreview")}
          >
            <X className="w-4 h-4" />
          </button>
        </div>
      </div>

      {/* Content area — HTML/slides/docx/xlsx fill height; markdown scrolls */}
      {document.format === "html" ||
      document.format === "slides" ||
      document.format === "docx" ||
      document.format === "xlsx" ? (
        <div className="flex-1 flex flex-col min-h-0">
          {loadState === "loading" && (
            <div className="flex items-center justify-center h-40 text-secondary">
              <Loader2 className="w-6 h-6 animate-spin" />
              <span className="ml-2 text-sm">{t("DocumentPreviewPanel.loading")}</span>
            </div>
          )}
          {loadState === "error" && (
            <div className="flex flex-col items-center justify-center h-40 text-secondary gap-2">
              <AlertCircle className="w-8 h-8 text-red-400" />
              <p className="text-sm">{t("DocumentPreviewPanel.loadFailed")}</p>
              <p className="text-xs opacity-60">{document.filename}</p>
            </div>
          )}
          {loadState === "loaded" && content !== null && (
            document.format === "slides" ? (
              <SlidesShadowDom content={content} />
            ) : (
              <HtmlShadowDom content={content} />
            )
          )}
        </div>
      ) : (
        <div className="flex-1 overflow-y-auto p-4">
          {loadState === "loading" && (
            <div className="flex items-center justify-center h-40 text-secondary">
              <Loader2 className="w-6 h-6 animate-spin" />
              <span className="ml-2 text-sm">{t("DocumentPreviewPanel.loading")}</span>
            </div>
          )}
          {loadState === "error" && (
            <div className="flex flex-col items-center justify-center h-40 text-secondary gap-2">
              <AlertCircle className="w-8 h-8 text-red-400" />
              <p className="text-sm">{t("DocumentPreviewPanel.loadFailed")}</p>
              <p className="text-xs opacity-60">{document.filename}</p>
            </div>
          )}
          {loadState === "loaded" && content !== null && (
            <div className="document-preview-content">
              <div className="prose prose-sm dark:prose-invert max-w-none">
                <MarkdownMessage text={content} />
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
