import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, Coins } from "lucide-react";
import { Section } from "@/components/ui/Section";
import {
  cloudLogin,
  cloudLogout,
  cloudStatus,
  cloudSubscription,
  cloudUsage,
  type CloudStatus,
  type CloudSubscription,
  type CloudUsage,
} from "@/lib/cloud";

/** Syscity Cloud login status + credit balance/usage (P2-10). Hidden when
 * cloud is disabled. */
export function CloudSection() {
  const { t } = useTranslation("settings");
  const [status, setStatus] = useState<CloudStatus | null>(null);
  const [sub, setSub] = useState<CloudSubscription | null>(null);
  const [usage, setUsage] = useState<CloudUsage | null>(null);

  useEffect(() => {
    cloudStatus().then(setStatus).catch(() => setStatus(null));
  }, []);

  useEffect(() => {
    if (!status?.logged_in) return;
    cloudSubscription().then(setSub).catch(() => setSub(null));
    cloudUsage(30).then(setUsage).catch(() => setUsage(null));
  }, [status]);

  if (!status?.enabled) return null;

  const user = status.user;
  const display = user?.name ?? user?.email ?? user?.id ?? t("CloudSection.cloudUser");
  const low = sub ? sub.overdrawn || sub.threshold_warn : false;

  return (
    <Section title={t("CloudSection.title")}>
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-1 sm:gap-0 px-3 py-2 rounded-lg bg-card">
        <div className="min-w-0">
          <p className="text-sm text-primary">
            {status.logged_in ? t("CloudSection.signedIn") : t("CloudSection.notSignedIn")}
          </p>
          {status.logged_in && (
            <p className="truncate text-xs text-secondary">{display}</p>
          )}
        </div>
        {status.logged_in ? (
          <button
            type="button"
            onClick={() => {
              void cloudLogout();
              setStatus({ ...status, logged_in: false, user: null });
              setSub(null);
              setUsage(null);
            }}
            className="rounded-lg border border-subtle px-3 py-1.5 text-sm font-semibold text-primary transition hover:border-primary-500 hover:text-primary-500"
          >
            {t("CloudSection.signOut")}
          </button>
        ) : (
          <button
            type="button"
            onClick={() => cloudLogin("github")}
            className="rounded-lg border border-subtle px-3 py-1.5 text-sm font-semibold text-primary transition hover:border-primary-500 hover:text-primary-500"
          >
            {t("CloudSection.signIn")}
          </button>
        )}
      </div>

      {status.logged_in && (
        <div className="mt-2 space-y-2">
          {/* Balance + plan */}
          <div
            className={`px-3 py-2 rounded-lg bg-card text-sm ${
              low ? "border border-amber-400/60" : ""
            }`}
          >
            <div className="flex items-center justify-between gap-2">
              <span className="inline-flex items-center gap-1.5 text-primary">
                <Coins size={14} className="text-primary-500" />
                {sub ? t("CloudSection.credits", { n: sub.balance.toLocaleString() }) : "…"}
              </span>
              <span className="text-xs text-secondary capitalize">
                {sub ? t("CloudSection.plan", { plan: sub.plan }) : ""}
              </span>
            </div>
            {low && (
              <p className="mt-1 flex items-center gap-1 text-xs text-amber-600 dark:text-amber-400">
                <AlertTriangle size={12} />
                {sub?.overdrawn
                  ? t("CloudSection.overdrawn")
                  : t("CloudSection.lowCredits")}
              </p>
            )}
          </div>

          {/* Usage summary */}
          {usage && (
            <div className="px-3 py-2 rounded-lg bg-card text-xs text-secondary">
              <div className="flex items-center justify-between mb-1.5">
                <span className="text-primary">
                  {t("CloudSection.usedThisMonth", { n: usage.month_credits.toLocaleString() })}
                </span>
                <span>{t("CloudSection.usageSummary", { calls: usage.total_calls, days: usage.days })}</span>
              </div>
              <div className="flex flex-wrap gap-1">
                {usage.by_category.map((c) => (
                  <span
                    key={c.category}
                    className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-sidebar"
                  >
                    {c.category} · {c.credits.toLocaleString()}
                  </span>
                ))}
                {usage.by_category.length === 0 && <span>{t("CloudSection.noUsage")}</span>}
              </div>
            </div>
          )}

          <p className="px-3 text-xs text-tertiary">
            {t("CloudSection.modelPickerHint")}
          </p>
        </div>
      )}
    </Section>
  );
}
