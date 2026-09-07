import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import type { EnvPreset, McpPreset, McpServer } from "@/components/settings/useSettingsData";
import { AddMcpForm } from "@/components/settings/AddMcpForm";
import { Section } from "@/components/ui/Section";
import { Toggle } from "@/components/ui/Toggle";

interface McpSettingsProps {
  transport: SyscityWebSocketTransport;
  mcpServers: McpServer[];
  mcpPresets: McpPreset[];
  actionLoading: string;
  onEnablePreset: (preset: McpPreset) => Promise<void>;
  onDisablePreset: (name: string) => Promise<void>;
  onConnect: (id: string) => Promise<void>;
  onDisconnect: (id: string) => Promise<void>;
  onRemove: (id: string) => Promise<void>;
  onOpenEnv: (preset: EnvPreset) => void;
  onRefreshMcp: () => Promise<void>;
}

export function McpSettings({
  transport,
  mcpServers,
  mcpPresets,
  actionLoading,
  onEnablePreset,
  onDisablePreset,
  onConnect,
  onDisconnect,
  onRemove,
  onOpenEnv,
  onRefreshMcp,
}: McpSettingsProps) {
  const { t } = useTranslation("settings");
  const [showAddMcp, setShowAddMcp] = useState(false);

  return (
    <div className="space-y-5">
      {mcpPresets.length > 0 && (
        <Section title={t("McpSettings.presets")}>
          <div className="grid grid-cols-1 sm:grid-cols-2 sm:grid-cols-3 md:grid-cols-2 sm:grid-cols-4 gap-2">
            {mcpPresets.map((p) => {
              const loading = actionLoading === p.name;
              return (
                <div
                  key={p.name}
                  className={`flex flex-col items-start gap-1 px-3 py-2.5 rounded-lg border text-left text-xs transition ${
                    p.enabled
                      ? "border-primary-400 bg-primary-100 dark:bg-primary-900/30"
                      : "border-subtle"
                  } ${loading ? "opacity-50" : ""}`}
                >
                  <div className="flex items-center gap-1.5 w-full">
                    {p.logo_url && (
                      <img src={p.logo_url} alt="" className="w-4 h-4 object-contain shrink-0" />
                    )}
                    <span className="font-medium text-sm flex-1">{p.display_name}</span>
                    <Toggle
                      variant="preset"
                      checked={p.enabled}
                      disabled={loading}
                      onChange={() => (p.enabled ? onDisablePreset(p.name) : onEnablePreset(p))}
                    />
                  </div>
                  <span className="text-[11px] leading-tight opacity-70 line-clamp-2">{p.description}</span>
                  {p.enabled && p.env?.length ? (
                    <button
                      type="button"
                      disabled={loading}
                      onClick={() => onOpenEnv(p)}
                      className="text-[10px] px-1.5 py-0.5 rounded bg-sidebar text-secondary hover:text-primary transition"
                    >
                      {t("McpSettings.configureTokens")}
                    </button>
                  ) : null}
                </div>
              );
            })}
          </div>
        </Section>
      )}
      <Section
        title={t("McpSettings.mcpServer")}
        right={
          <button onClick={() => setShowAddMcp(true)} className="text-xs px-2 py-1 rounded bg-primary-500 text-white hover:bg-primary-600 transition">
            {t("McpSettings.add")}
          </button>
        }
      >
        {showAddMcp && (
          <AddMcpForm
            transport={transport}
            onAdded={() => {
              setShowAddMcp(false);
              onRefreshMcp();
            }}
          />
        )}
        {mcpServers.length === 0 ? (
          <div className="text-sm text-secondary">{t("McpSettings.none")}</div>
        ) : (
          <div className="space-y-2">
            {mcpServers.map((srv) => (
              <div key={srv.id} className="flex items-center justify-between px-3 py-2 rounded-lg bg-card">
                <div className="flex items-center gap-3">
                  <span className="text-sm text-primary font-medium">
                    {srv.id.charAt(0).toUpperCase() + srv.id.slice(1)}
                  </span>
                  <span className="text-xs px-1.5 py-0.5 rounded bg-sidebar text-secondary uppercase">{srv.transport}</span>
                  {srv.connected ? (
                    <span className="text-xs px-1.5 py-0.5 rounded bg-primary-100 dark:bg-primary-900/30 text-primary-700 dark:text-primary-400">{t("McpSettings.connected")}</span>
                  ) : (
                    <span className="text-xs px-1.5 py-0.5 rounded bg-sidebar text-secondary">{t("McpSettings.disconnected")}</span>
                  )}
                </div>
                <div className="flex items-center gap-2">
                  {srv.connected ? (
                    <button onClick={() => onDisconnect(srv.id)} disabled={actionLoading === srv.id} className="text-xs px-2 py-0.5 rounded bg-sidebar text-secondary hover:bg-black/[0.06] dark:hover:bg-white/[0.08] transition">
                      {actionLoading === srv.id ? "..." : t("McpSettings.disconnect")}
                    </button>
                  ) : (
                    <button onClick={() => onConnect(srv.id)} disabled={actionLoading === srv.id} className="text-xs px-2 py-0.5 rounded bg-primary-100 dark:bg-primary-900/30 text-primary-700 dark:text-primary-400 hover:bg-primary-200 dark:hover:bg-primary-900/50 transition">
                      {actionLoading === srv.id ? "..." : t("McpSettings.connect")}
                    </button>
                  )}
                  <button onClick={() => onRemove(srv.id)} disabled={actionLoading === srv.id} className="p-1 rounded hover:bg-red-100 dark:hover:bg-red-900/30 text-secondary/60 hover:text-red-600 dark:hover:text-red-400 transition" title={t("McpSettings.remove")}>
                    <svg className="w-4 h-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </Section>
    </div>
  );
}
