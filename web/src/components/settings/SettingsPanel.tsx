import { X } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { useSettingsData } from "@/components/settings/useSettingsData";
import { GeneralSettings } from "@/components/settings/GeneralSettings";
import { ModelsSettings } from "@/components/settings/ModelsSettings";
import { AgentsSettings } from "@/components/settings/AgentsSettings";
import { ChannelsSettings } from "@/components/settings/ChannelsSettings";
import { ToolsSettings } from "@/components/settings/ToolsSettings";
import { McpSettings } from "@/components/settings/McpSettings";
import { SkillsSettings } from "@/components/settings/SkillsSettings";
import { MarketplaceSettings } from "@/components/settings/MarketplaceSettings";
import { EvalDashboard } from "@/components/settings/EvalDashboard";
import { JobsSettings } from "@/components/settings/JobsSettings";
import { DevicesSettings } from "@/components/settings/DevicesSettings";
import { ConnectionSettings } from "@/components/settings/ConnectionSettings";
import { LogsSettings } from "@/components/settings/LogsSettings";
import { McpAuthModal } from "@/components/settings/McpAuthModal";
import { McpEnvModal } from "@/components/settings/McpEnvModal";

interface SettingsPanelProps {
  transport: SyscityWebSocketTransport;
  onClose: () => void;
  /** Tab to open on (e.g. "marketplace" when launched from the sidebar). */
  initialTab?: string;
  /** Marketplace expert summon: open a session bound to the agent
   *  (with an optional starter prompt). */
  onSummonExpert?: (agentId: string, starterPrompt?: string) => void;
  /** Marketplace skill follow-up: open a fresh welcome page with a
   *  pre-filled composer. */
  onNewSessionWithDraft?: (draft: string) => void;
}

export function SettingsPanel({
  transport,
  onClose,
  initialTab = "general",
  onSummonExpert,
  onNewSessionWithDraft,
}: SettingsPanelProps) {
  const { t } = useTranslation("settings");
  const d = useSettingsData(transport, initialTab);

  return (
    <div className="flex-1 flex flex-col overflow-hidden bg-page">
      {/* Header */}
      <div className="flex items-center justify-between px-4 md:px-5 py-3 border-b border-subtle shrink-0">
        <h2 className="text-base font-semibold text-primary">{t("SettingsPanel.title")}</h2>
        <button
          onClick={onClose}
          className="p-1.5 rounded-lg hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
          title={t("SettingsPanel.backToChat")}
        >
          <X className="w-5 h-5" />
        </button>
      </div>

      {d.loading ? (
        <div className="flex-1 flex items-center justify-center text-secondary">
          <div className="w-6 h-6 border-2 border-subtle border-t-primary-500 rounded-full animate-spin mb-3 mr-3" />
          {t("SettingsPanel.loading")}
        </div>
      ) : (
        <div className="flex-1 flex flex-col md:flex-row overflow-hidden">
          {/* Tabs: horizontal strip on mobile, left sidebar on md+ */}
          <div className="flex md:flex-col gap-0.5 overflow-x-auto md:overflow-y-auto md:overflow-x-hidden border-b md:border-b-0 md:border-r border-subtle shrink-0 py-2 md:py-3 px-2 md:w-44">
            {d.tabs.map((tab) => (
              <button key={tab.id} onClick={() => d.setActiveTab(tab.id)} className={`${d.tabCls(tab.id)} shrink-0`}>
                {t(`SettingsPanel.tab.${tab.id}`, { defaultValue: tab.label })}
              </button>
            ))}
          </div>

          {/* Right content */}
          <div className="flex-1 overflow-y-auto px-4 md:px-5 py-4">
            {d.activeTab === "general" && (
              <GeneralSettings transport={transport} config={d.config} models={d.models} currentTheme={d.currentTheme} update={d.update} />
            )}
            {d.activeTab === "models" && (
              <ModelsSettings
                transport={transport}
                models={d.models}
                config={d.config}
                modelActionLoading={d.modelActionLoading}
                showAddModel={d.showAddModel}
                onToggleAdd={() => d.setShowAddModel(!d.showAddModel)}
                onRefresh={d.refreshModels}
                onSetDefault={d.handleSetDefaultModel}
                onRemove={d.handleRemoveModel}
              />
            )}
            {d.activeTab === "agents" && (
              <AgentsSettings
                agentRegistry={d.agentRegistry}
                selectedAgentId={d.selectedAgentId}
                onSelectAgent={d.handleSelectAgent}
                selectedAgentDetail={d.selectedAgentDetail}
                agentDetailLoading={d.agentDetailLoading}
                defaultAgent={d.da}
                agentOverrides={d.config.agent_overrides?.[d.selectedAgentId]}
                models={d.models}
                agentModels={d.config.agent_models || {}}
                update={d.update}
                updateAgentParam={d.updateAgentParam}
                resetAgentParam={d.resetAgentParam}
                resetAgentParams={d.resetAgentParams}
              />
            )}
            {d.activeTab === "channels" && (
              <ChannelsSettings
                transport={transport}
                channels={d.channels}
                actionLoading={d.channelActionLoading}
                onToggle={d.handleToggleChannel}
                onRemove={d.handleRemoveChannel}
                onRefresh={d.refreshConfig}
              />
            )}
            {d.activeTab === "tools" && (
              <ToolsSettings config={d.config} update={d.update} />
            )}
            {d.activeTab === "mcp" && (
              <McpSettings
                transport={transport}
                mcpServers={d.mcpServers}
                mcpPresets={d.mcpPresets}
                actionLoading={d.mcpActionLoading}
                onEnablePreset={d.handleEnablePreset}
                onDisablePreset={d.handleDisablePreset}
                onConnect={d.handleConnectMcp}
                onDisconnect={d.handleDisconnectMcp}
                onRemove={d.handleRemoveMcp}
                onOpenEnv={(preset) => d.setEnvModal({ preset, values: {} })}
                onRefreshMcp={d.refreshMcp}
              />
            )}
            {d.activeTab === "marketplace" && (
              <MarketplaceSettings
                onSummonExpert={onSummonExpert}
                onNewSessionWithDraft={onNewSessionWithDraft}
              />
            )}
            {d.activeTab === "skills" && (
              <SkillsSettings transport={transport} skills={d.skills} onRefresh={d.refreshSkills} />
            )}
            {d.activeTab === "eval" && (
              <EvalDashboard transport={transport} />
            )}
            {d.activeTab === "jobs" && (
              <JobsSettings crons={d.crons} />
            )}
            {d.activeTab === "devices" && (
              <DevicesSettings transport={transport} showToast={d.showToast} />
            )}
            {d.activeTab === "logs" && (
              <LogsSettings transport={transport} />
            )}
            {d.activeTab === "connection" && <ConnectionSettings />}
          </div>
        </div>
      )}

      <McpAuthModal authModal={d.authModal} onCancel={d.handleCancelAuth} />
      <McpEnvModal envModal={d.envModal} setEnvModal={d.setEnvModal} submitEnv={d.submitEnv} />

      {/* Toast notification */}
      {d.toast && (
        <div
          className={`fixed bottom-6 left-1/2 -translate-x-1/2 z-50 px-4 py-2 rounded-lg text-sm shadow-lg transition-all ${
            d.toast.type === "success"
              ? "bg-green-600 text-white"
              : "bg-red-600 text-white"
          }`}
        >
          {d.toast.message}
        </div>
      )}
    </div>
  );
}
