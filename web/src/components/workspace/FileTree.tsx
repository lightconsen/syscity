import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  AlertCircle,
  ChevronDown,
  ChevronRight,
  File,
  FileCode,
  Folder,
  FolderOpen,
  Loader2,
} from "lucide-react";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";

export interface WorkspaceEntry {
  name: string;
  path: string;
  kind: "dir" | "file";
  size: number;
  modified?: number;
}

type DirState =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "loaded"; entries: WorkspaceEntry[] };

/** Extensions rendered with the code highlighter icon. */
const CODE_EXTS = new Set([
  "ts", "tsx", "js", "jsx", "json", "rs", "py", "toml", "yaml", "yml", "sh",
  "css", "html", "sql", "go", "java", "c", "cpp", "h", "swift", "kt",
]);

function extOf(name: string): string {
  const idx = name.lastIndexOf(".");
  return idx > 0 ? name.slice(idx + 1).toLowerCase() : "";
}

interface FileTreeProps {
  transport: SyscityWebSocketTransport;
  agentId?: string;
  selectedPath?: string | null;
  onSelectFile: (path: string) => void;
  onRootResolved?: (root: string) => void;
}

/**
 * Lazy recursive file tree over the workspace.list WS method. Only one
 * directory level is fetched per expand; loaded children are cached so
 * collapse/re-expand is instant. The component is keyed by agent upstream,
 * so all state resets on session/agent switch.
 */
export function FileTree({
  transport,
  agentId,
  selectedPath,
  onSelectFile,
  onRootResolved,
}: FileTreeProps) {
  const { t } = useTranslation("workspace");
  // Key: directory path relative to the workspace root ("" = root).
  const [children, setChildren] = useState<Record<string, DirState>>({});
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const loadDir = useCallback(
    async (dirPath: string) => {
      setChildren((prev) => ({ ...prev, [dirPath]: { status: "loading" } }));
      try {
        const res = await transport.workspaceList(agentId, dirPath);
        setChildren((prev) => ({
          ...prev,
          [dirPath]: { status: "loaded", entries: res.entries },
        }));
        if (dirPath === "") onRootResolved?.(res.root);
      } catch (err) {
        setChildren((prev) => ({
          ...prev,
          [dirPath]: {
            status: "error",
            message: err instanceof Error ? err.message : t("FileTree.failedToLoad"),
          },
        }));
      }
    },
    [transport, agentId, onRootResolved, t]
  );

  useEffect(() => {
    loadDir("");
  }, [loadDir]);

  const toggleDir = useCallback(
    (dirPath: string) => {
      const isExpanded = !!expanded[dirPath];
      setExpanded((prev) => {
        const next = { ...prev };
        if (isExpanded) {
          delete next[dirPath];
        } else {
          next[dirPath] = true;
        }
        return next;
      });
      if (!isExpanded && !children[dirPath]) {
        loadDir(dirPath);
      }
    },
    [expanded, children, loadDir]
  );

  const rowClass =
    "w-full flex items-center gap-1.5 py-1 pr-2 text-sm text-primary hover:bg-black/5 dark:hover:bg-white/5 transition text-left rounded-md";

  const renderLevel = (dirPath: string, depth: number): React.ReactNode => {
    const pad = { paddingLeft: 8 + depth * 14 };
    const node = children[dirPath];
    if (!node || node.status === "loading") {
      return (
        <div className="flex items-center gap-1.5 py-1 pr-2 text-sm text-secondary" style={pad}>
          <Loader2 className="w-3.5 h-3.5 animate-spin shrink-0" />
          <span className="text-xs">{t("FileTree.loading")}</span>
        </div>
      );
    }
    if (node.status === "error") {
      return (
        <div className="flex items-center gap-1.5 py-1 pr-2 text-sm text-red-400" style={pad}>
          <AlertCircle className="w-3.5 h-3.5 shrink-0" />
          <span className="text-xs truncate">{node.message}</span>
          <button
            type="button"
            className="text-xs underline shrink-0"
            onClick={() => loadDir(dirPath)}
          >
            {t("FileTree.retry")}
          </button>
        </div>
      );
    }
    if (node.entries.length === 0) {
      return (
        <div className="py-1 pr-2 text-xs text-secondary" style={pad}>
          {t("FileTree.empty")}
        </div>
      );
    }
    return node.entries.map((entry) => {
      if (entry.kind === "dir") {
        const isOpen = !!expanded[entry.path];
        return (
          <div key={entry.path}>
            <button
              type="button"
              className={rowClass}
              style={{ paddingLeft: 8 + depth * 14 }}
              onClick={() => toggleDir(entry.path)}
            >
              {isOpen ? (
                <ChevronDown className="w-3.5 h-3.5 shrink-0 text-secondary" />
              ) : (
                <ChevronRight className="w-3.5 h-3.5 shrink-0 text-secondary" />
              )}
              {isOpen ? (
                <FolderOpen className="w-4 h-4 shrink-0 text-primary-500" />
              ) : (
                <Folder className="w-4 h-4 shrink-0 text-primary-500" />
              )}
              <span className="truncate">{entry.name}</span>
            </button>
            {isOpen && renderLevel(entry.path, depth + 1)}
          </div>
        );
      }
      const isSelected = selectedPath === entry.path;
      const Icon = CODE_EXTS.has(extOf(entry.name)) ? FileCode : File;
      return (
        <button
          key={entry.path}
          type="button"
          className={`${rowClass} ${isSelected ? "bg-black/5 dark:bg-white/10" : ""}`}
          style={{ paddingLeft: 8 + depth * 14 + 18 }}
          onClick={() => onSelectFile(entry.path)}
          title={entry.path}
        >
          <Icon className="w-4 h-4 shrink-0 text-secondary" />
          <span className="truncate">{entry.name}</span>
        </button>
      );
    });
  };

  return <div className="flex-1 overflow-y-auto py-1 px-1">{renderLevel("", 0)}</div>;
}
