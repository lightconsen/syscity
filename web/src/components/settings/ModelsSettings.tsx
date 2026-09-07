import type { ModelInfo, SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { useTranslation } from "react-i18next";
import type { SyscityConfig } from "@/components/settings/useSettingsData";
import { AddModelForm } from "@/components/settings/AddModelForm";
import { Section } from "@/components/ui/Section";
import { Button } from "@/components/ui/Button";
import { ProviderLogo } from "@/components/ui/ProviderLogo";

interface ModelsSettingsProps {
  transport: SyscityWebSocketTransport;
  models: ModelInfo[];
  config: SyscityConfig;
  modelActionLoading: string;
  showAddModel: boolean;
  onToggleAdd: () => void;
  onRefresh: () => Promise<void>;
  onSetDefault: (id: string) => Promise<void>;
  onRemove: (id: string) => Promise<void>;
}

/** One configured provider: logo + name + model count, with compact rows per model. */
function ProviderCard({
  provider,
  providerName,
  ms,
  config,
  modelActionLoading,
  onSetDefault,
  onRemove,
}: {
  provider: string;
  providerName: string;
  ms: ModelInfo[];
  config: SyscityConfig;
  modelActionLoading: string;
  onSetDefault: (id: string) => Promise<void>;
  onRemove: (id: string) => Promise<void>;
}) {
  const { t } = useTranslation("settings");
  const hasDefault = ms.some((m) => config.model === m.id);
  return (
    <div className="rounded-lg border border-subtle bg-card overflow-hidden">
      <div className="flex items-center gap-2 px-3 py-2 bg-black/[0.02] dark:bg-white/[0.03]">
        <ProviderLogo provider={provider} name={providerName} className="w-5 h-5" />
        <span className="text-sm text-primary font-medium truncate">{providerName}</span>
        <span className="text-xs text-secondary whitespace-nowrap">
          {t("ModelsSettings.modelCount", { count: ms.length })}
        </span>
        {hasDefault && (
          <span className="ml-auto text-xs px-2 py-0.5 rounded-full bg-primary-100 dark:bg-primary-900/30 text-primary-700 dark:text-primary-400">
            {t("ModelsSettings.default")}
          </span>
        )}
      </div>
      <div className="divide-y divide-black/[0.03] dark:divide-white/[0.04]">
        {ms.map((m) => (
          <div key={m.id} className="flex items-center justify-between gap-2 px-3 py-2">
            <span className="text-sm text-primary truncate">{m.name}</span>
            <div className="flex items-center gap-2 shrink-0">
              {config.model === m.id ? (
                <span className="text-xs px-2 py-0.5 rounded-full bg-primary-100 dark:bg-primary-900/30 text-primary-700 dark:text-primary-400">
                  {t("ModelsSettings.default")}
                </span>
              ) : (
                <button
                  onClick={() => onSetDefault(m.id)}
                  disabled={modelActionLoading === `default_${m.id}`}
                  className="text-xs px-2 py-0.5 rounded-full bg-sidebar text-secondary hover:bg-primary-100 dark:hover:bg-primary-900/30 hover:text-primary-700 dark:hover:text-primary-400 transition"
                >
                  {modelActionLoading === `default_${m.id}` ? "..." : t("ModelsSettings.setDefault")}
                </button>
              )}
              <button
                onClick={() => onRemove(m.id)}
                disabled={modelActionLoading === m.id}
                className="p-1 rounded hover:bg-red-100 dark:hover:bg-red-900/30 text-secondary/60 hover:text-red-600 dark:hover:text-red-400 transition"
                title={t("ModelsSettings.remove")}
              >
                <svg className="w-4 h-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                  <line x1="18" y1="6" x2="6" y2="18" />
                  <line x1="6" y1="6" x2="18" y2="18" />
                </svg>
              </button>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

export function ModelsSettings({
  transport,
  models,
  config,
  modelActionLoading,
  showAddModel,
  onToggleAdd,
  onRefresh,
  onSetDefault,
  onRemove,
}: ModelsSettingsProps) {
  const { t } = useTranslation("settings");
  // Group configured models by provider so each provider renders once.
  const byProvider = new Map<string, ModelInfo[]>();
  for (const m of models) {
    const list = byProvider.get(m.provider) || [];
    list.push(m);
    byProvider.set(m.provider, list);
  }

  return (
    <div className="space-y-5">
      <Section
        title={t("ModelsSettings.availableModels")}
        right={
          <Button variant="primary-sm" onClick={onToggleAdd}>
            {showAddModel ? t("ModelsSettings.cancel") : t("ModelsSettings.add")}
          </Button>
        }
      >
        {showAddModel && (
          <div className="mb-4">
            <AddModelForm transport={transport} models={models} globalDefaultModel={config.model} onAdded={onRefresh} />
          </div>
        )}

        {models.length === 0 ? (
          <div className="text-sm text-secondary">{t("ModelsSettings.noModels")}</div>
        ) : (
          <div className="space-y-3">
            {Array.from(byProvider.entries()).map(([provider, ms]) => (
              <ProviderCard
                key={provider}
                provider={provider}
                providerName={ms[0]?.provider_name || provider}
                ms={ms}
                config={config}
                modelActionLoading={modelActionLoading}
                onSetDefault={onSetDefault}
                onRemove={onRemove}
              />
            ))}
          </div>
        )}
      </Section>
    </div>
  );
}
