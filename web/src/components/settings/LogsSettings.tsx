import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";

interface LogsSettingsProps {
  transport: SyscityWebSocketTransport;
}

export function LogsSettings({ transport }: LogsSettingsProps) {
  const { t } = useTranslation("settings");
  const [logLines, setLogLines] = useState<string[]>([]);
  const [logsSubscribed, setLogsSubscribed] = useState(false);
  const logListRef = useRef<HTMLDivElement>(null);

  // Subscribe while the Logs tab is mounted; unsubscribe on unmount.
  useEffect(() => {
    transport.subscribeLogs();
    setLogsSubscribed(true);
    return () => {
      transport.unsubscribeLogs();
      setLogsSubscribed(false);
      setLogLines([]);
    };
  }, [transport]);

  // Listen for log.line events
  useEffect(() => {
    const unsub = transport.onEvent((evt) => {
      if (evt.event === "log.line") {
        const line = (evt.payload?.line as string) || "";
        setLogLines((prev) => [...prev, line]);
      }
    });
    return unsub;
  }, [transport]);

  // Auto-scroll logs to bottom
  useEffect(() => {
    if (logListRef.current) {
      logListRef.current.scrollTop = logListRef.current.scrollHeight;
    }
  }, [logLines]);

  return (
    <div className="flex flex-col h-full">
      <section className="flex flex-col flex-1">
        <div className="flex items-center justify-between mb-2">
          <h3 className="text-xs font-semibold text-secondary uppercase tracking-wider">{t("LogsSettings.title")}</h3>
          <span className={`text-[10px] px-2 py-0.5 rounded-full ${logsSubscribed ? 'bg-green-100 text-green-700 dark:bg-green-900/30 dark:text-green-400' : 'bg-sidebar text-secondary/70'}`}>
            {logsSubscribed ? t("LogsSettings.live") : t("LogsSettings.disconnected")}
          </span>
        </div>
        <div
          ref={logListRef}
          className="bg-sidebar rounded-lg h-[90vh] overflow-y-auto font-mono text-[11px] leading-4 p-3"
        >
          {logLines.length === 0 && (
            <div className="text-secondary/50 text-center py-20">
              {logsSubscribed ? t("LogsSettings.waiting") : t("LogsSettings.clickToConnect")}
            </div>
          )}
          {logLines.map((line, i) => (
            <div key={i} className="text-secondary whitespace-pre-wrap break-all py-0.5 border-b border-subtle last:border-0">
              {line}
            </div>
          ))}
        </div>
        <div className="flex gap-2 mt-2">
          <button
            onClick={() => setLogLines([])}
            className="px-3 py-1.5 text-xs bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 rounded-md text-secondary transition-colors"
          >
            {t("LogsSettings.clear")}
          </button>
          <button
            onClick={() => {
              const blob = new Blob([logLines.join("\n")], { type: "text/plain" });
              const url = URL.createObjectURL(blob);
              const a = document.createElement("a");
              a.href = url;
              a.download = `syscity-logs-${new Date().toISOString().slice(0, 19)}.txt`;
              a.click();
              URL.revokeObjectURL(url);
            }}
            disabled={logLines.length === 0}
            className="px-3 py-1.5 text-xs bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 rounded-md text-secondary transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {t("LogsSettings.download")}
          </button>
        </div>
      </section>
    </div>
  );
}
