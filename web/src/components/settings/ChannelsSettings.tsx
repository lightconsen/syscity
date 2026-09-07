import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import type { ChannelConfig } from "@/components/settings/useSettingsData";
import { AddChannelForm } from "@/components/settings/AddChannelForm";
import { Section } from "@/components/ui/Section";
import { Button } from "@/components/ui/Button";

interface ChannelsSettingsProps {
  transport: SyscityWebSocketTransport;
  channels: ChannelConfig[];
  actionLoading: string;
  onToggle: (name: string, enabled: boolean) => Promise<void>;
  onRemove: (name: string) => Promise<void>;
  onRefresh: () => Promise<void>;
}

export function ChannelsSettings({ transport, channels, actionLoading, onToggle, onRemove, onRefresh }: ChannelsSettingsProps) {
  const { t } = useTranslation("settings");
  const [showAddChannel, setShowAddChannel] = useState(false);

  return (
    <div className="space-y-5">
      <Section
        title={t("ChannelsSettings.title")}
        right={
          <Button variant="primary-sm" onClick={() => setShowAddChannel(!showAddChannel)}>
            {showAddChannel ? t("ChannelsSettings.cancel") : t("ChannelsSettings.add")}
          </Button>
        }
      >
        {showAddChannel && (
          <AddChannelForm
            transport={transport}
            onAdded={() => {
              setShowAddChannel(false);
              onRefresh();
            }}
          />
        )}

        {channels.length === 0 ? (
          <div className="text-sm text-secondary">{t("ChannelsSettings.empty")}</div>
        ) : (
          <div className="space-y-2">
            {channels.map((ch) => (
              <div key={ch.name} className="flex items-center justify-between px-3 py-2 rounded-lg bg-card">
                <div className="flex items-center gap-3">
                  <span className="text-sm text-primary font-medium">{ch.name}</span>
                  <span className="text-xs px-1.5 py-0.5 rounded bg-sidebar text-secondary uppercase">{ch.channel_type}</span>
                  {ch.agent_id && (
                    <span className="text-xs text-secondary font-mono">{ch.agent_id}</span>
                  )}
                  {ch.dm_policy && ch.dm_policy !== "open" && (
                    <span className="text-xs px-1.5 py-0.5 rounded bg-blue-100 dark:bg-blue-900/30 text-blue-700 dark:text-blue-400">{ch.dm_policy}</span>
                  )}
                  {ch.require_mention && (
                    <span className="text-xs px-1.5 py-0.5 rounded bg-purple-100 dark:bg-purple-900/30 text-purple-700 dark:text-purple-400">mention</span>
                  )}
                  {ch.has_credentials && (
                    <span className="text-xs px-1.5 py-0.5 rounded bg-amber-100 dark:bg-amber-900/30 text-amber-700 dark:text-amber-400">auth</span>
                  )}
                </div>
                <div className="flex items-center gap-2">
                  <button
                    onClick={() => onToggle(ch.name, !ch.enabled)}
                    disabled={actionLoading === ch.name}
                    className={`text-xs px-2 py-0.5 rounded-full transition ${
                      ch.enabled
                        ? "bg-primary-100 dark:bg-primary-900/30 text-primary-700 dark:text-primary-400 hover:bg-primary-200 dark:hover:bg-primary-900/50"
                        : "bg-sidebar text-secondary hover:bg-black/[0.06] dark:hover:bg-white/[0.08]"
                    }`}
                  >
                    {actionLoading === ch.name ? "..." : ch.enabled ? t("ChannelsSettings.enabled") : t("ChannelsSettings.disabled")}
                  </button>
                  <button
                    onClick={() => onRemove(ch.name)}
                    disabled={actionLoading === ch.name}
                    className="p-1 rounded hover:bg-red-100 dark:hover:bg-red-900/30 text-secondary/60 hover:text-red-600 dark:hover:text-red-400 transition"
                    title={t("ChannelsSettings.remove")}
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
        )}
      </Section>
    </div>
  );
}
