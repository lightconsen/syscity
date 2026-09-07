import type { Dispatch, SetStateAction } from "react";
import { useTranslation } from "react-i18next";
import type { EnvModalState } from "@/components/settings/useSettingsData";

interface McpEnvModalProps {
  envModal: EnvModalState | null;
  setEnvModal: Dispatch<SetStateAction<EnvModalState | null>>;
  submitEnv: () => Promise<void>;
}

export function McpEnvModal({ envModal, setEnvModal, submitEnv }: McpEnvModalProps) {
  const { t } = useTranslation("settings");
  if (!envModal) return null;
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div className="bg-card rounded-xl p-6 max-w-md w-full mx-4 shadow-xl">
        <h3 className="text-sm font-semibold mb-1">{t("McpEnvModal.title", { name: envModal.preset.display_name })}</h3>
        <p className="text-xs text-secondary mb-4">
          {t("McpEnvModal.body")}
        </p>
        <div className="space-y-3">
          {envModal.preset.env.map((v) => (
            <div key={v.name}>
              <label className="block text-xs text-secondary mb-1">
                {v.name}
                {v.required && <span className="text-red-500"> *</span>}
              </label>
              <input
                type="password"
                placeholder="••••••••"
                value={envModal.values[v.name] ?? ""}
                onChange={(e) =>
                  setEnvModal((m) =>
                    m ? { ...m, values: { ...m.values, [v.name]: e.target.value } } : m
                  )
                }
                className="w-full rounded-lg border border-subtle bg-card px-3 py-1.5 text-sm text-primary placeholder-gray-400 focus:outline-none focus:ring-2 focus:ring-primary-500/20"
              />
              {v.description && (
                <p className="text-[10px] text-secondary/70 mt-0.5">{v.description}</p>
              )}
            </div>
          ))}
        </div>
        {envModal.error && (
          <div className="mt-3 text-xs text-red-600 dark:text-red-400 break-words">{envModal.error}</div>
        )}
        <div className="flex gap-2 mt-5">
          <button
            onClick={() => submitEnv()}
            disabled={envModal.saving}
            className="flex-1 px-4 py-2 text-xs font-medium rounded-lg bg-primary-600 text-white hover:opacity-90 transition-opacity disabled:opacity-50"
          >
            {envModal.saving ? t("McpEnvModal.validating") : t("McpEnvModal.saveEnable")}
          </button>
          <button
            onClick={() => setEnvModal(null)}
            disabled={envModal.saving}
            className="px-4 py-2 text-xs font-medium rounded-lg bg-sidebar text-secondary hover:text-primary transition-colors"
          >
            {t("McpEnvModal.cancel")}
          </button>
        </div>
      </div>
    </div>
  );
}
