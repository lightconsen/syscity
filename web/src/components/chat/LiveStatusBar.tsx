import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

interface LiveStatusBarProps {
  liveStatus: { status: "thinking" | "tool_calling"; toolName?: string };
  startTime: number;
}

export function LiveStatusBar({ liveStatus, startTime }: LiveStatusBarProps) {
  const [elapsed, setElapsed] = useState(0);
  const { t } = useTranslation("chat");

  useEffect(() => {
    const id = setInterval(() => setElapsed(Date.now() - startTime), 500);
    return () => clearInterval(id);
  }, [startTime]);

  const label =
    liveStatus.status === "tool_calling"
      ? t("LiveStatusBar.runningTool", { tool: liveStatus.toolName || "tool" })
      : t("LiveStatusBar.thinking");

  return (
    <div className="mt-2 flex items-center gap-2 text-xs text-secondary animate-pulse">
      <div className="w-1.5 h-1.5 rounded-full bg-primary-500" />
      <span>{label}</span>
      <span className="font-mono">({(elapsed / 1000).toFixed(1)}s)</span>
    </div>
  );
}
