import { useState } from "react";
import { useTranslation } from "react-i18next";
import { MarkdownMessage } from "./MarkdownMessage";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";

interface ToolCallPartProps {
  toolName: string;
  args: Record<string, unknown>;
  result?: unknown;
  data?: unknown;
  isError?: boolean;
  transport?: SyscityWebSocketTransport;
  nonCollapsible?: boolean;
}

/** Detect macOS Accessibility permission error from tool result. */
function isPermissionError(result: unknown): result is { needs_permission: true; error: string } {
  return (
    typeof result === "object" &&
    result !== null &&
    "needs_permission" in result &&
    (result as Record<string, unknown>).needs_permission === true
  );
}

/** Heuristic: does this string look like markdown content? */
function looksLikeMarkdown(text: string): boolean {
  const markdownPatterns = [
    /^\s*#{1,6}\s+/m, // headers
    /!\[.*?\]\(.*?\)/, // images
    /\[.*?\]\(.*?\)/, // links
    /(\*\*|__)(?=\S)(.*?\S)\1/m, // bold
    /(\*|_)(?=\S)(.*?\S)\1/m, // italic
    /^\s*[-*+]\s+/m, // lists
    /^\s*```/m, // code blocks
    /^\s*>\s+/m, // blockquote
    /\|.*\|.*\|/, // tables
  ];
  return markdownPatterns.some((re) => re.test(text));
}

/** Detect tool result that reports an error by shape or string prefix. */
function isErrorResult(result: unknown): boolean {
  if (typeof result === "string") {
    return result.startsWith("Error:") || result.toLowerCase().startsWith("error");
  }
  if (typeof result !== "object" || result === null) return false;
  return "error" in result;
}

export function ToolCallPart({ toolName, args, result, data, isError: isErrorProp, transport, nonCollapsible }: ToolCallPartProps) {
  const { t } = useTranslation("common");
  const [expanded, setExpanded] = useState(true);
  const [requesting, setRequesting] = useState(false);
  const [requestDone, setRequestDone] = useState(false);

  const isError = isErrorProp || isErrorResult(result);

  const needsPermission = isPermissionError(data);

  const statusColor = isError
    ? "border-l-red-400 bg-red-50/40 dark:bg-red-950/20 text-red-700 dark:text-red-400"
    : result !== undefined
    ? "border-l-emerald-400 bg-emerald-50/40 dark:bg-emerald-950/20 text-emerald-700 dark:text-emerald-400"
    : "border-l-primary-400 bg-primary-50/40 dark:bg-primary-900/15 text-primary-700 dark:text-primary-400";

  const toolNameBadgeClass = "bg-primary-100 dark:bg-primary-900/40 text-primary-700 dark:text-primary-300";

  const resultBoxClass = isError
    ? "bg-red-50 dark:bg-red-950/20 text-red-800 dark:text-red-200"
    : "bg-black/5 dark:bg-white/5";

  const statusText = isError
    ? t("ToolCallPart.error")
    : result !== undefined
    ? t("ToolCallPart.done")
    : t("ToolCallPart.running");

  const statusBadge = (
    <span
      className={`text-[10px] font-medium ${
        isError
          ? "px-1.5 py-0.5 rounded bg-red-500 text-white"
          : result !== undefined
          ? "px-1.5 py-0.5 rounded bg-emerald-500 text-white"
          : "px-1.5 py-0.5 rounded bg-primary-500 text-white"
      }`}
    >
      {statusText}
    </span>
  );

  const resultString =
    typeof result === "string" ? result : result !== undefined ? JSON.stringify(result, null, 2) : undefined;
  const renderAsMarkdown = typeof result === "string" && looksLikeMarkdown(resultString || "");

  const handleRequestPermission = async () => {
    if (!transport) return;
    setRequesting(true);
    const res = await transport.requestMacosAccessibility();
    setRequesting(false);
    if (res) {
      setRequestDone(true);
    }
  };

  const content = (
    <div className="px-3 py-2 text-xs">
      <div className="mb-2">
        <div className="text-[10px] font-semibold uppercase tracking-wider opacity-60 mb-1">{t("ToolCallPart.arguments")}</div>
        <pre className="bg-black/5 dark:bg-white/5 rounded-lg p-2 overflow-x-auto max-w-full whitespace-pre-wrap font-mono text-[11px]">
          {JSON.stringify(args, null, 2)}
        </pre>
      </div>
      {needsPermission ? (
        <div className="rounded-lg bg-amber-50/60 dark:bg-amber-950/20 p-3">
          <div className="flex items-start gap-2">
            <svg className="w-4 h-4 text-amber-600 dark:text-amber-400 shrink-0 mt-0.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 15v2m-6 4h12a2 2 0 002-2v-6a2 2 0 00-2-2H6a2 2 0 00-2 2v6a2 2 0 002 2zm10-10V7a4 4 0 00-8 0v4h8z" />
            </svg>
            <div className="flex-1 min-w-0">
              <div className="font-semibold text-amber-800 dark:text-amber-300 mb-1">
                {t("ToolCallPart.macPermTitle")}
              </div>
              <div className="text-amber-700 dark:text-amber-400/80 mb-2 leading-relaxed">
                {t("ToolCallPart.macPermBody")}
              </div>
              {!requestDone ? (
                <button
                  onClick={handleRequestPermission}
                  disabled={requesting}
                  className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-md bg-amber-600 hover:bg-amber-700 disabled:opacity-50 text-white text-[11px] font-medium transition"
                >
                  {requesting ? (
                    <>
                      <span className="w-3 h-3 border-2 border-white/30 border-t-white rounded-full animate-spin" />
                      {t("ToolCallPart.requesting")}
                    </>
                  ) : (
                    <>
                      <svg className="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                        <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
                        <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
                      </svg>
                      {t("ToolCallPart.openSystemSettings")}
                    </>
                  )}
                </button>
              ) : (
                <div className="text-green-700 dark:text-green-400 text-[11px] font-medium">
                  {t("ToolCallPart.macPermDone")}
                </div>
              )}
            </div>
          </div>
        </div>
      ) : result !== undefined && (
        <div>
          <div className="text-[10px] font-semibold uppercase tracking-wider opacity-60 mb-1">{t("ToolCallPart.result")}</div>
          {renderAsMarkdown ? (
            <div className={`rounded-lg p-2 overflow-x-auto max-w-full ${resultBoxClass}`}>
              <MarkdownMessage text={resultString || ""} />
            </div>
          ) : (
            <pre className={`rounded-lg p-2 overflow-x-auto max-w-full whitespace-pre-wrap font-mono text-[11px] ${resultBoxClass}`}>
              {resultString}
            </pre>
          )}
        </div>
      )}
    </div>
  );

  if (nonCollapsible) {
    const [showContent, setShowContent] = useState(false);

    return (
      <div className="my-3">
        <button
          type="button"
          onClick={() => setShowContent(!showContent)}
          className="w-full flex items-center gap-2 text-[11px] font-medium text-secondary hover:text-primary transition"
        >
          <svg
            className={`w-3 h-3 transition-transform ${showContent ? "rotate-90" : ""}`}
            fill="none"
            stroke="currentColor"
            viewBox="0 0 24 24"
          >
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 5l7 7-7 7" />
          </svg>
          <span className={`font-mono px-1.5 py-0.5 rounded ${toolNameBadgeClass}`}>
            {toolName}
          </span>
          <span className="ml-auto flex items-center gap-1.5 text-[10px]">
            {result === undefined && !isError && (
              <span className="relative flex h-2 w-2">
                <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-primary-400 opacity-75" />
                <span className="relative inline-flex rounded-full h-2 w-2 bg-primary-500" />
              </span>
            )}
            {statusBadge}
          </span>
        </button>
        {showContent && (
          <>
            <hr className="border-subtle my-2" />
            {content}
          </>
        )}
      </div>
    );
  }

  return (
    <div className={`my-2 rounded-lg border-l-4 ${statusColor}`}>
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full flex items-center gap-2 px-3 py-2 text-xs font-medium hover:opacity-80 transition"
      >
        <svg
          className={`w-3.5 h-3.5 transition-transform ${expanded ? "rotate-90" : ""}`}
          fill="none"
          stroke="currentColor"
          viewBox="0 0 24 24"
        >
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 5l7 7-7 7" />
        </svg>
        <span className={`font-mono px-1.5 py-0.5 rounded ${toolNameBadgeClass}`}>
          {toolName}
        </span>
        <span className="ml-auto flex items-center gap-1.5 text-[10px] opacity-70">
          {result === undefined && !isError && (
            <span className="relative flex h-2 w-2">
              <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-primary-400 opacity-75" />
              <span className="relative inline-flex rounded-full h-2 w-2 bg-primary-500" />
            </span>
          )}
          {statusText}
        </span>
      </button>
      {expanded && content}
    </div>
  );
}
