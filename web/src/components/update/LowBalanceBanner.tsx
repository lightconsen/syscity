import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, X, XCircle } from "lucide-react";
import { cloudStatus, cloudSubscription } from "@/lib/cloud";

const DISMISS_KEY = "syscity_credits_banner_dismissed";

/** Dismissal severity — a worse level re-shows the banner after dismissal. */
type Level = "warn" | "overdrawn";
const LEVEL_RANK: Record<Level, number> = { warn: 1, overdrawn: 2 };

function readDismissedLevel(): Level | null {
  try {
    const v = localStorage.getItem(DISMISS_KEY);
    return v === "warn" || v === "overdrawn" ? v : null;
  } catch {
    return null;
  }
}

/**
 * A2: low-balance / overdraft banner. Self-fetches cloud status (gating on
 * enabled + logged_in) and the subscription; `threshold_warn` renders an
 * amber hint, `overdrawn` a red one — both with a recharge button deep
 * linking the cloud console. Dismissal persists per severity and a more
 * severe level (overdrawn after warn) re-shows it. Polls every 5 min so a
 * top-up clears it without a reload.
 */
export function LowBalanceBanner() {
  const { t } = useTranslation("update");
  const [level, setLevel] = useState<Level | null>(null);
  const [consoleUrl, setConsoleUrl] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState<Level | null>(readDismissedLevel);

  const check = useCallback(async () => {
    try {
      const status = await cloudStatus();
      if (!status.enabled || !status.logged_in) return;
      if (status.console_url) setConsoleUrl(status.console_url.replace(/\/$/, ""));
      const sub = await cloudSubscription();
      setLevel(sub.overdrawn ? "overdrawn" : sub.threshold_warn ? "warn" : null);
    } catch {
      /* transient — keep last known state */
    }
  }, []);

  useEffect(() => {
    void check();
    const timer = window.setInterval(() => void check(), 5 * 60_000);
    return () => window.clearInterval(timer);
  }, [check]);

  // Hidden when dismissed at an equal-or-worse level.
  const visible =
    level !== null && LEVEL_RANK[level] > LEVEL_RANK[dismissed ?? "warn"];
  if (!visible) return null;

  const overdrawn = level === "overdrawn";
  const Icon = overdrawn ? XCircle : AlertTriangle;
  const tone = overdrawn
    ? "border-red-300/60 dark:border-red-500/30 bg-red-50 dark:bg-red-900/20"
    : "border-amber-300/60 dark:border-amber-500/30 bg-amber-50 dark:bg-amber-900/20";
  const iconTone = overdrawn
    ? "text-red-500"
    : "text-amber-500";

  const dismiss = () => {
    setDismissed(level);
    try {
      localStorage.setItem(DISMISS_KEY, level);
    } catch {
      /* ignore */
    }
  };

  return (
    <div
      className={`shrink-0 px-4 py-2.5 border-b ${tone} flex items-start gap-3`}
    >
      <Icon className={`w-4 h-4 shrink-0 mt-0.5 ${iconTone}`} />
      <div className="text-xs text-primary flex-1 min-w-0 leading-relaxed">
        {overdrawn ? t("LowBalanceBanner.overdrawn") : t("LowBalanceBanner.low")}
      </div>
      {consoleUrl && (
        <a
          href={consoleUrl}
          target="_blank"
          rel="noreferrer"
          className="shrink-0 text-xs px-2 py-1 rounded-md bg-primary-500 hover:bg-primary-600 text-white font-medium transition"
        >
          {t("LowBalanceBanner.recharge")}
        </a>
      )}
      <button
        type="button"
        onClick={dismiss}
        className="p-1 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition shrink-0"
        aria-label={t("LowBalanceBanner.dismiss")}
      >
        <X className="w-4 h-4" />
      </button>
    </div>
  );
}
