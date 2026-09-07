import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Cloud, X } from "lucide-react";

const HINT_KEY = "syscity_cloud_enabled_hint";

/**
 * First-login cloud guidance (docs/cloud-integration.md「首次登录引导」): shown
 * right after a successful cloud login, pointing the user at what just became
 * available — cloud models/search, the full marketplace, usage/credits, and
 * how to upload knowledge-base documents. Dismissing clears it permanently;
 * it reappears only after another login.
 */
export function CloudEnabledBanner() {
  const { t } = useTranslation("update");
  const [visible, setVisible] = useState(() => {
    try {
      return localStorage.getItem(HINT_KEY) === "1";
    } catch {
      return false;
    }
  });

  if (!visible) return null;

  return (
    <div className="shrink-0 px-4 py-2.5 border-b border-subtle bg-primary-50 dark:bg-primary-900/20 flex items-start gap-3">
      <Cloud className="w-4 h-4 text-primary-500 shrink-0 mt-0.5" />
      <div className="text-xs text-primary flex-1 min-w-0 leading-relaxed">
        {t("CloudEnabledBanner.body")}{" "}
        <code className="text-[10px] px-1 py-0.5 rounded bg-black/5 dark:bg-white/10">Cloud KB</code>.
      </div>
      <button
        type="button"
        onClick={() => {
          setVisible(false);
          try {
            localStorage.removeItem(HINT_KEY);
          } catch {
            /* ignore */
          }
        }}
        className="p-1 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition shrink-0"
        aria-label={t("CloudEnabledBanner.dismiss")}
      >
        <X className="w-4 h-4" />
      </button>
    </div>
  );
}
