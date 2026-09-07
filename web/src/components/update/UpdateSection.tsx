import { useTranslation } from "react-i18next";
import { Loader2 } from "lucide-react";
import { useUpdate, isTauri } from "@/hooks/useUpdate";
import { Section } from "@/components/ui/Section";
import { Button } from "@/components/ui/Button";

const PHASE_KEY: Record<string, string> = {
  idle: "UpdateBanner.idle",
  checking: "UpdateBanner.checking",
  downloading: "UpdateBanner.downloading",
  verifying: "UpdateBanner.verifying",
  applying: "UpdateBanner.applying",
  restarting: "UpdateBanner.restarting",
  error: "UpdateBanner.error",
};

/** Row helper mirroring the existing Gateway info rows. */
function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-1 sm:gap-0 px-3 py-2 rounded-lg bg-card">
      <span className="text-sm text-secondary">{label}</span>
      <span className="text-sm text-primary font-mono break-all sm:text-right">{children}</span>
    </div>
  );
}

/**
 * About/Update section for the settings General tab: current + latest version,
 * a "Check for updates" button, and a one-click update with progress.
 */
export function UpdateSection() {
  const { t } = useTranslation("update");
  const { status, progress, busy, error, message, runUpdate, checkNow } = useUpdate();

  const phase = progress?.phase || "idle";
  const active = busy || (phase !== "idle" && phase !== "error");
  const enabled = status?.enabled ?? false;

  return (
    <Section title={t("UpdateSection.title")}>
      <div className="space-y-2">
        <Row label={t("UpdateSection.currentVersion")}>v{status?.current || "—"}</Row>
        <Row label={t("UpdateSection.latestVersion")}>
          {status?.update_available ? `v${status.latest || "?"}` : t("UpdateBanner.upToDate")}
        </Row>

        <div className="flex flex-wrap items-center gap-2 px-3 py-2">
          <Button variant="primary-md" onClick={() => runUpdate()} disabled={!enabled || active}>
            {active ? (
              <span className="inline-flex items-center gap-2">
                <Loader2 className="w-3.5 h-3.5 animate-spin" />
                {t(PHASE_KEY[phase] ?? "UpdateBanner.idle")} {progress?.percent ?? 0}%
              </span>
            ) : isTauri() ? (
              t("UpdateSection.checkAndUpdate")
            ) : (
              t("UpdateSection.updateNow")
            )}
          </Button>
          {!isTauri() && (
            <Button variant="ghost" onClick={() => checkNow()} disabled={!enabled || active}>
              {t("UpdateSection.checkForUpdates")}
            </Button>
          )}
        </div>

        {/* Progress bar while downloading/applying. */}
        {active && phase === "downloading" && (
          <div className="px-3 pb-2">
            <div className="h-1.5 rounded-full bg-black/10 dark:bg-white/10 overflow-hidden">
              <div
                className="h-full bg-primary-500 rounded-full transition-all"
                style={{ width: `${progress?.percent ?? 0}%` }}
              />
            </div>
          </div>
        )}

        {message && !active && <div className="px-3 text-xs text-secondary">{message}</div>}
        {error && <div className="px-3 text-xs text-red-500">{error}</div>}
        {!enabled && status && (
          <div className="px-3 text-xs text-secondary">
            {t("UpdateSection.disabledNote")}
          </div>
        )}
        {isTauri() && (
          <div className="px-3 text-[11px] text-secondary/70">
            {t("UpdateSection.tauriNote")}
          </div>
        )}
      </div>
    </Section>
  );
}
