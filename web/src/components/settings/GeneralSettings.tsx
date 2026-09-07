import type { ModelInfo, SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { useTranslation } from "react-i18next";
import { setAppLang, currentLangChoice } from "@/i18n";
import { useThemeStore } from "@/stores/themeStore";
import { useSpeechStore, type SpeechLangChoice } from "@/stores/speechStore";
import type { SyscityConfig } from "@/components/settings/useSettingsData";
import { Section } from "@/components/ui/Section";
import { Select } from "@/components/ui/Select";
import { Input } from "@/components/ui/Input";
import { Toggle } from "@/components/ui/Toggle";
import { ProviderLogo } from "@/components/ui/ProviderLogo";
import { CloudSection } from "@/components/settings/CloudSection";
import { UpdateSection } from "@/components/update/UpdateSection";

interface GeneralSettingsProps {
  transport: SyscityWebSocketTransport;
  config: SyscityConfig;
  models: ModelInfo[];
  currentTheme: string;
  update: (path: string, value: unknown) => Promise<void>;
}

export function GeneralSettings({ transport, config, models, currentTheme, update }: GeneralSettingsProps) {
  const { t } = useTranslation("common");
  const langChoice = currentLangChoice();
  const hb = config.heartbeat || {};
  const si = transport.getServerInfo();
  const speechLang = useSpeechStore((s) => s.lang);

  return (
    <div className="space-y-5">
      <Section title={t("GeneralSettings.gateway")}>
        <div className="space-y-2">
          <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-1 sm:gap-0 px-3 py-2 rounded-lg bg-card">
            <span className="text-sm text-secondary">{t("GeneralSettings.url")}</span>
            <span className="text-sm text-primary font-mono break-all sm:text-right">{transport.getGatewayUrl() || "—"}</span>
          </div>
          <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-1 sm:gap-0 px-3 py-2 rounded-lg bg-card">
            <span className="text-sm text-secondary">{t("GeneralSettings.version")}</span>
            <span className="text-sm text-primary font-mono break-all sm:text-right">{si.version || "—"}</span>
          </div>
          <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-1 sm:gap-0 px-3 py-2 rounded-lg bg-card">
            <span className="text-sm text-secondary">{t("GeneralSettings.connection")}</span>
            <span className="text-sm text-primary font-mono break-all sm:text-right">{si.conn_id || "—"}</span>
          </div>
          <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-1 sm:gap-0 px-3 py-2 rounded-lg bg-card">
            <span className="text-sm text-secondary">{t("GeneralSettings.features")}</span>
            <span className="text-sm text-primary break-all sm:text-right">{(si.features || []).join(", ") || "—"}</span>
          </div>
          <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-1 sm:gap-0 px-3 py-2 rounded-lg bg-card">
            <span className="text-sm text-secondary">{t("GeneralSettings.authMode")}</span>
            <span className="text-sm text-primary font-mono break-all capitalize sm:text-right">{String((config as Record<string, unknown>).auth_mode || "—")}</span>
          </div>
          <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-1 sm:gap-0 px-3 py-2 rounded-lg bg-card">
            <span className="text-sm text-secondary">{t("GeneralSettings.scopes")}</span>
            <span className="text-sm text-primary break-all sm:text-right">{(si.scopes_granted || []).join(", ") || "—"}</span>
          </div>
        </div>
      </Section>

      <CloudSection />

      <UpdateSection />

      <Section title={t("GeneralSettings.modelProvider")}>
        <div className="space-y-3">
          <Select
            label={t("GeneralSettings.defaultModel")}
            labelClassName="block text-sm text-secondary mb-1"
            value={config.model || ""}
            onChange={(e) => update("model", e.target.value)}
          >
            {models.map((m) => (
              <option key={m.id} value={m.id}>
                {m.provider_name || m.provider} - {m.name}
              </option>
            ))}
          </Select>
          <div>
            <label className="block text-sm text-secondary mb-1">{t("GeneralSettings.provider")}</label>
            <div className="flex items-center gap-2 w-full rounded-lg border border-subtle bg-sidebar px-3 py-2 text-sm text-secondary cursor-not-allowed">
              {(() => {
                const owner = models.find((m) => m.id === config.model);
                return owner ? (
                  <ProviderLogo provider={owner.provider} name={owner.provider_name} className="w-4 h-4" />
                ) : null;
              })()}
              <span>{config.model_provider || "—"}</span>
            </div>
          </div>
        </div>
      </Section>

      <Section title={t("GeneralSettings.appearance")}>
        <div className="space-y-3">
          <div>
            <label className="block text-sm text-secondary mb-1">{t("GeneralSettings.themeMode")}</label>
            <div className="flex gap-2">
              {(["system", "light", "dark"] as const).map((m) => (
                <button
                  key={m}
                  onClick={() => useThemeStore.getState().setTheme(m)}
                  className={`px-3 py-1.5 rounded-lg border text-sm transition capitalize ${
                    currentTheme === m
                      ? "bg-primary-100 dark:bg-primary-900/30 text-primary-700 dark:text-primary-400 border-primary-400 font-medium"
                      : "border-subtle text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
                  }`}
                >
                  {t(`GeneralSettings.theme${m[0].toUpperCase()}${m.slice(1)}`)}
                </button>
              ))}
            </div>
          </div>
          <div>
            <label className="block text-sm text-secondary mb-1">{t("GeneralSettings.language")}</label>
            <Select
              value={langChoice}
              labelClassName="hidden"
              onChange={(e) => setAppLang(e.target.value as "system" | "en" | "zh")}
            >
              <option value="system">{t("GeneralSettings.langSystem")}</option>
              <option value="en">English</option>
              <option value="zh">简体中文</option>
            </Select>
          </div>
        </div>
      </Section>

      <Section title={t("GeneralSettings.voice")}>
        <Select
          label={t("GeneralSettings.recognitionLanguage")}
          labelClassName="block text-sm text-secondary mb-1"
          value={speechLang}
          onChange={(e) => useSpeechStore.getState().setLang(e.target.value as SpeechLangChoice)}
        >
          <option value="system">{t("GeneralSettings.langSystem")}</option>
          <option value="zh-CN">中文（普通话）</option>
          <option value="zh-TW">中文（繁體）</option>
          <option value="en-US">English (US)</option>
          <option value="ja-JP">日本語</option>
          <option value="ko-KR">한국어</option>
        </Select>
        <div className="mt-1 text-[11px] text-secondary/70">
          {t("GeneralSettings.voiceHint")}
        </div>
      </Section>

      <Section title={t("GeneralSettings.heartbeat")}>
        <div className="space-y-3">
          <Toggle label={t("GeneralSettings.enableHeartbeat")} checked={!!hb.enabled} onChange={() => update("heartbeat.enabled", !hb.enabled)} />
          <Input
            label={t("GeneralSettings.intervalSeconds")}
            labelClassName="block text-sm text-secondary mb-1"
            type="number"
            value={hb.interval_seconds ?? 300}
            onChange={(e) => update("heartbeat.interval_seconds", parseInt(e.target.value))}
          />
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
            <Input
              label={t("GeneralSettings.activeFrom")}
              labelClassName="block text-sm text-secondary mb-1"
              type="text"
              value={hb.active_hours_start || ""}
              onChange={(e) => update("heartbeat.active_hours_start", e.target.value)}
            />
            <Input
              label={t("GeneralSettings.activeTo")}
              labelClassName="block text-sm text-secondary mb-1"
              type="text"
              value={hb.active_hours_end || ""}
              onChange={(e) => update("heartbeat.active_hours_end", e.target.value)}
            />
          </div>
        </div>
      </Section>

      <Section title={t("GeneralSettings.tokenUsage")}>
        <div className="text-sm text-secondary">{t("GeneralSettings.tokenUsageSoon")}</div>
      </Section>
    </div>
  );
}
