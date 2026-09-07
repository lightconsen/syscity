import { useTranslation } from "react-i18next";

interface JobsSettingsProps {
  crons: Array<Record<string, unknown>>;
}

export function JobsSettings({ crons }: JobsSettingsProps) {
  const { t } = useTranslation("settings");
  return (
    <div className="space-y-5">
      <section>
        <h3 className="text-xs font-semibold text-secondary uppercase tracking-wider mb-2">{t("JobsSettings.title", { count: crons.length })}</h3>
        {crons.length === 0 ? (
          <div className="text-sm text-secondary">{t("JobsSettings.empty")}</div>
        ) : (
          <div className="space-y-2">
            {crons.map((job, i) => {
              const j = job as Record<string, unknown>;
              const target = j.target as Record<string, unknown> | undefined;
              const targetType = target?.type as string | undefined;
              const jobState = j.state as Record<string, unknown> | undefined;
              const nextRun = jobState?.next_run_at as string | undefined;
              const lastRun = jobState?.last_run_at as string | undefined;
              const agentId = target?.agent_id as string | undefined;
              const command = target?.command as string | undefined;
              const prompt = target?.prompt as string | undefined;
              return (
                <div key={i} className="px-3 py-2 rounded-lg bg-card">
                  <div className="flex items-center justify-between">
                    <span className="text-sm text-primary font-medium">{(j.name as string) || t("JobsSettings.unnamed")}</span>
                    <span className={`text-xs px-2 py-0.5 rounded-full ${j.enabled ? "bg-primary-100 dark:bg-primary-900/30 text-primary-700 dark:text-primary-400" : "bg-sidebar text-secondary"}`}>
                      {j.enabled ? t("JobsSettings.enabled") : t("JobsSettings.disabled")}
                    </span>
                  </div>
                  <div className="mt-1.5 space-y-1">
                    {(() => {
                      const sched = j.schedule as Record<string, unknown> | string | undefined;
                      const expr = typeof sched === "string" ? sched : (sched as Record<string, unknown> | undefined)?.expression as string | undefined;
                      if (!expr) return null;
                      return (
                        <div className="flex items-center gap-2">
                          <span className="text-[10px] uppercase tracking-wider text-secondary/70 w-16 shrink-0">{t("JobsSettings.schedule")}</span>
                          <span className="text-xs text-secondary font-mono">{expr}</span>
                        </div>
                      );
                    })()}
                    {nextRun && (
                      <div className="flex items-center gap-2">
                        <span className="text-[10px] uppercase tracking-wider text-secondary/70 w-16 shrink-0">{t("JobsSettings.nextRun")}</span>
                        <span className="text-xs text-secondary">{new Date(nextRun).toLocaleString()}</span>
                      </div>
                    )}
                    {lastRun && (
                      <div className="flex items-center gap-2">
                        <span className="text-[10px] uppercase tracking-wider text-secondary/70 w-16 shrink-0">{t("JobsSettings.lastRun")}</span>
                        <span className="text-xs text-secondary">{new Date(lastRun).toLocaleString()}</span>
                      </div>
                    )}
                    <div className="flex items-center gap-2">
                      <span className="text-[10px] uppercase tracking-wider text-secondary/70 w-16 shrink-0">{t("JobsSettings.target")}</span>
                      {targetType === "shell" ? (
                        <span className="text-xs px-1.5 py-0.5 rounded bg-sidebar text-secondary">{t("JobsSettings.shell")}</span>
                      ) : targetType === "agent" ? (
                        <span className="text-xs px-1.5 py-0.5 rounded bg-purple-100 dark:bg-purple-900/30 text-purple-700 dark:text-purple-400">{t("JobsSettings.agent")}</span>
                      ) : (
                        <span className="text-xs text-secondary">{targetType || t("JobsSettings.unknown")}</span>
                      )}
                    </div>
                    {targetType === "agent" && agentId && (
                      <div className="flex items-center gap-2">
                        <span className="text-[10px] uppercase tracking-wider text-secondary/70 w-16 shrink-0">{t("JobsSettings.agent")}</span>
                        <span className="text-xs text-secondary font-mono">{agentId}</span>
                      </div>
                    )}
                    {targetType === "shell" && command && (
                      <div className="flex items-start gap-2">
                        <span className="text-[10px] uppercase tracking-wider text-secondary/70 w-16 shrink-0">{t("JobsSettings.command")}</span>
                        <span className="text-xs text-secondary font-mono break-all">{command}</span>
                      </div>
                    )}
                    {targetType === "agent" && prompt && (
                      <div className="flex items-start gap-2">
                        <span className="text-[10px] uppercase tracking-wider text-secondary/70 w-16 shrink-0">{t("JobsSettings.prompt")}</span>
                        <span className="text-xs text-secondary line-clamp-2">{prompt}</span>
                      </div>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </section>
    </div>
  );
}
