import { useState } from "react";
import { useTranslation } from "react-i18next";
import { X, Download } from "lucide-react";
import { useUpdate } from "@/hooks/useUpdate";
import { Button } from "@/components/ui/Button";

const PHASE_KEY: Record<string, string> = {
  checking: "UpdateBanner.checking",
  downloading: "UpdateBanner.downloading",
  verifying: "UpdateBanner.verifying",
  applying: "UpdateBanner.applying",
  restarting: "UpdateBanner.restarting",
};

const DISMISSED_KEY = "syscity_update_banner_dismissed";

/**
 * Global strip shown when a newer syscity release is available. Clicking
 * "Update" starts the flow: in the plain web app it POSTs to the daemon's
 * `/api/v1/update` (binary replace + restart), in the desktop webview it
 * routes to the Tauri updater command.
 */
export function UpdateBanner() {
  const { t } = useTranslation("update");
  const { status, progress, busy, error, runUpdate } = useUpdate();
  const [dismissed, setDismissed] = useState<string | null>(() =>
    localStorage.getItem(DISMISSED_KEY)
  );

  if (!status || !status.enabled || !status.update_available) return null;
  if (dismissed === status.latest) return null;

  const phase = progress?.phase || "idle";
  const percent = progress?.percent ?? 0;
  const active = busy || (phase !== "idle" && phase !== "error");

  return (
    <div className="shrink-0 px-4 py-2 border-b border-subtle bg-primary-50 dark:bg-primary-900/20 flex items-center gap-3">
      <Download className="w-4 h-4 text-primary-500 shrink-0" />
      <span className="text-xs text-primary flex-1 min-w-0">
        {active ? (
          <span className="flex items-center gap-2">
            {PHASE_KEY[phase] ? t(PHASE_KEY[phase]) : t("UpdateBanner.updating")}
            <span className="hidden sm:inline text-secondary">{percent}%</span>
            {phase === "downloading" && (
              <span className="w-24 h-1.5 rounded-full bg-black/10 dark:bg-white/10 overflow-hidden">
                <span
                  className="block h-full bg-primary-500 rounded-full transition-all"
                  style={{ width: `${percent}%` }}
                />
              </span>
            )}
          </span>
        ) : (
          <span>
            {t("UpdateBanner.available", { latest: status.latest || "?", current: status.current })}
          </span>
        )}
      </span>
      {error && (
        <span className="text-xs text-red-500 truncate max-w-[40%]" title={error}>
          {error}
        </span>
      )}
      {!active && !error && (
        <Button variant="primary-sm" onClick={() => runUpdate()}>
          {t("UpdateBanner.update")}
        </Button>
      )}
      <button
        type="button"
        onClick={() => {
          setDismissed(status.latest || null);
          if (status.latest) localStorage.setItem(DISMISSED_KEY, status.latest);
        }}
        className="p-1 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
        aria-label={t("UpdateBanner.dismiss")}
      >
        <X className="w-4 h-4" />
      </button>
    </div>
  );
}
