import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

interface LiveStatusBarProps {
  liveStatus: { status: "thinking" | "tool_calling" | "awaiting_approval"; toolName?: string };
  /**
   * When the turn started, in epoch ms. Must be stable for a given turn: the
   * counter re-anchors when this value changes, so a caller that recomputes it
   * on every render (e.g. `Date.now() - 5000`) makes the elapsed time climb
   * and then snap backwards.
   */
  startTime: number;
}

export function LiveStatusBar({ liveStatus, startTime }: LiveStatusBarProps) {
  const { t } = useTranslation("chat");
  // A live message always carries a stamp; 0 means "unknown" (e.g. a message
  // restored from a snapshot), and counting from the Unix epoch would print a
  // nonsense number. Count from mount in that case.
  const anchorRef = useRef(startTime > 0 ? startTime : Date.now());
  const [elapsed, setElapsed] = useState(0);

  // Re-anchor when a different turn lands in this slot (the message list
  // recycles component instances by index).
  if (startTime > 0 && anchorRef.current !== startTime) {
    anchorRef.current = startTime;
  }

  useEffect(() => {
    const tick = () => setElapsed(Math.max(0, Date.now() - anchorRef.current));
    tick();
    const id = setInterval(tick, 500);
    return () => clearInterval(id);
  }, [startTime]);

  const label =
    liveStatus.status === "tool_calling"
      ? t("LiveStatusBar.runningTool", { tool: liveStatus.toolName || "tool" })
      : liveStatus.status === "awaiting_approval"
        ? t("LiveStatusBar.awaitingApproval")
        : t("LiveStatusBar.thinking");

  return (
    <div
      className={`mt-2 flex items-center gap-2 text-xs text-secondary${liveStatus.status === "awaiting_approval" ? "" : " animate-pulse"}`}
      data-testid="live-status"
      data-status={liveStatus.status}
    >
      <div className="w-1.5 h-1.5 rounded-full bg-primary-500" />
      <span>{label}</span>
      <span className="font-mono">({(elapsed / 1000).toFixed(1)}s)</span>
    </div>
  );
}
