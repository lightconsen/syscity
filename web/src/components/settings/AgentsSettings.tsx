import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { ModelInfo } from "@/SyscityWebSocketTransport";
import { Section } from "@/components/ui/Section";
import { Select } from "@/components/ui/Select";
import { Input } from "@/components/ui/Input";
import { Toggle } from "@/components/ui/Toggle";
import type { AgentParamOverrides } from "@/components/settings/useSettingsData";

interface AgentRegistryItem {
  id: string;
  display_name: string;
  emoji?: string;
  is_valid: boolean;
  has_heartbeat: boolean;
}

interface AgentDetail {
  agent_id: string;
  busy: boolean;
  status: string;
  config: Record<string, unknown> | null;
  personality: Record<string, unknown> | null;
}

interface AgentsSettingsProps {
  agentRegistry: AgentRegistryItem[];
  selectedAgentId: string;
  onSelectAgent: (id: string) => void;
  selectedAgentDetail: AgentDetail | null;
  agentDetailLoading: boolean;
  defaultAgent: Record<string, unknown>;
  agentOverrides?: AgentParamOverrides;
  models: ModelInfo[];
  agentModels: Record<string, string>;
  update: (path: string, value: unknown) => Promise<void>;
  updateAgentParam: (agentId: string, field: string, value: unknown) => Promise<void>;
  resetAgentParam: (agentId: string, field: string) => Promise<void>;
  resetAgentParams: (agentId: string) => Promise<void>;
}

export function AgentsSettings({
  agentRegistry,
  selectedAgentId,
  onSelectAgent,
  selectedAgentDetail,
  agentDetailLoading,
  defaultAgent,
  agentOverrides,
  models,
  agentModels,
  update,
  updateAgentParam,
  resetAgentParam,
  resetAgentParams,
}: AgentsSettingsProps) {
  const { t } = useTranslation("settings");
  const [paramTab, setParamTab] = useState<"general" | "prompt">("general");

  const agentId = selectedAgentId || "default";
  const isDefault = agentId === "default";
  const ov: AgentParamOverrides = agentOverrides ?? {};
  const da = defaultAgent;

  const num = (v: unknown): number | undefined => (typeof v === "number" ? v : undefined);

  // Effective value shown in each control: per-agent override when set,
  // otherwise the global default (default_agent), otherwise a built-in fallback.
  const eff = {
    temperature: ov.temperature ?? num(da.temperature) ?? 0.7,
    max_tokens: ov.max_tokens ?? num(da.max_tokens) ?? 2048,
    max_turns: ov.max_turns ?? num(da.max_turns) ?? null,
    max_concurrent_tools: ov.max_concurrent_tools ?? num(da.max_concurrent_tools) ?? 5,
    max_context_tokens: ov.max_context_tokens ?? num(da.max_context_tokens) ?? 4096,
    workspace_only: ov.workspace_only ?? (da.workspace_only as boolean | undefined) ?? true,
  };

  const isOverridden = (field: keyof AgentParamOverrides) =>
    !isDefault && ov[field] !== null && ov[field] !== undefined;
  const anyOverride = (Object.keys(ov) as Array<keyof AgentParamOverrides>).some(
    (k) => ov[k] !== null && ov[k] !== undefined,
  );

  const writeField = (field: string, value: unknown) => updateAgentParam(agentId, field, value);

  /** Empty input clears the override for named agents (inherit); for the
   * default agent it writes null where the field supports it. */
  const writeNumber = (field: string, raw: string, allowNull = false) => {
    if (raw === "") {
      if (isDefault) {
        if (allowNull) void updateAgentParam(agentId, field, null);
      } else {
        void resetAgentParam(agentId, field);
      }
      return;
    }
    const v = parseInt(raw, 10);
    if (!Number.isNaN(v)) void writeField(field, v);
  };

  /** Small "inherits global" badge / reset button shown next to a field label. */
  const overrideHint = (field: keyof AgentParamOverrides) => {
    if (isDefault) return null;
    if (isOverridden(field)) {
      return (
        <button
          type="button"
          onClick={() => void resetAgentParam(agentId, field)}
          className="text-[11px] text-primary-600 dark:text-primary-400 hover:underline"
          title={t("AgentsSettings.resetToGlobal")}
        >
          {t("AgentsSettings.reset")}
        </button>
      );
    }
    return <span className="text-[10px] text-secondary/60">{t("AgentsSettings.inheritsGlobal")}</span>;
  };

  const paramTabCls = (id: string) =>
    `px-3 py-1.5 text-sm rounded-t-md transition border-b-2 -mb-px ${
      paramTab === id
        ? "border-primary-500 text-primary-700 dark:text-primary-400 font-medium"
        : "border-transparent text-secondary hover:text-primary"
    }`;

  return (
    <div className="space-y-5">
      <Section title={t("AgentsSettings.selectAgent")}>
        {agentRegistry.length === 0 ? (
          <div className="text-sm text-secondary">{t("AgentsSettings.noAgents")}</div>
        ) : (
          <Select
            value={selectedAgentId}
            onChange={(e) => onSelectAgent(e.target.value)}
          >
            {agentRegistry.map((a) => (
              <option key={a.id} value={a.id}>
                {a.display_name || a.id}
              </option>
            ))}
          </Select>
        )}
      </Section>

      <section>
        <div className="flex gap-1 border-b border-subtle mb-3">
          <button type="button" className={paramTabCls("general")} onClick={() => setParamTab("general")}>
            {t("AgentsSettings.tabGeneral")}
          </button>
          <button type="button" className={paramTabCls("prompt")} onClick={() => setParamTab("prompt")}>
            {t("AgentsSettings.tabPrompt")}
          </button>
        </div>

        {paramTab === "general" && (
          <div className="space-y-5">
            {/* Model */}
            <div>
              <h4 className="text-xs font-semibold text-secondary uppercase tracking-wider mb-2">{t("AgentsSettings.model")}</h4>
              <Select
                value={agentModels[selectedAgentId] ?? ""}
                onChange={(e) => update(`agent_models.${selectedAgentId}`, e.target.value || null)}
              >
                <option value="">{t("AgentsSettings.globalDefault")}</option>
                {models.map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.provider_name || m.provider} - {m.name}
                  </option>
                ))}
              </Select>
              <div className="mt-1 text-[11px] text-secondary/70">
                {t("AgentsSettings.modelHint", { agent: selectedAgentId })}
              </div>
            </div>

            {agentDetailLoading && (
              <div className="text-sm text-secondary flex items-center gap-2">
                <div className="w-4 h-4 border-2 border-subtle border-t-primary-500 rounded-full animate-spin" />
                {t("AgentsSettings.loadingDetails")}
              </div>
            )}

            {selectedAgentDetail && !agentDetailLoading && (
              <div className="space-y-4">
                {/* Agent header */}
                <div className="flex items-center gap-3">
                  <span className="text-sm font-medium text-primary">
                    {selectedAgentDetail.personality
                      ? String((selectedAgentDetail.personality as Record<string, unknown>).display_name ?? selectedAgentDetail.agent_id)
                      : selectedAgentDetail.agent_id}
                  </span>
                  <span className="text-xs text-secondary/70 font-mono">({selectedAgentDetail.agent_id})</span>
                </div>

                {/* Personality */}
                {selectedAgentDetail.personality && (
                  <div>
                    <h4 className="text-xs font-semibold text-secondary uppercase tracking-wider mb-2">{t("AgentsSettings.personality")}</h4>
                    <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
                      <div className="px-3 py-2 rounded-lg bg-card">
                        <div className="text-[10px] uppercase tracking-wider text-secondary/70">{t("AgentsSettings.displayName")}</div>
                        <div className="text-sm text-primary">{String((selectedAgentDetail.personality as Record<string, unknown>).display_name ?? "—")}</div>
                      </div>
                      <div className="px-3 py-2 rounded-lg bg-card">
                        <div className="text-[10px] uppercase tracking-wider text-secondary/70">{t("AgentsSettings.valid")}</div>
                        <div className="text-sm text-primary">{(selectedAgentDetail.personality as Record<string, unknown>).is_valid ? t("AgentsSettings.yes") : t("AgentsSettings.no")}</div>
                      </div>
                    </div>
                    <div className="mt-2 flex flex-wrap gap-1.5">
                      {Boolean((selectedAgentDetail.personality as Record<string, unknown>).has_heartbeat) && (
                        <span className="text-xs px-2 py-0.5 rounded-full bg-amber-100 dark:bg-amber-900/30 text-amber-700 dark:text-amber-400">{t("AgentsSettings.heartbeat")}</span>
                      )}
                      {Boolean((selectedAgentDetail.personality as Record<string, unknown>).has_soul) && (
                        <span className="text-xs px-2 py-0.5 rounded-full bg-purple-100 dark:bg-purple-900/30 text-purple-700 dark:text-purple-400">{t("AgentsSettings.soul")}</span>
                      )}
                      {Boolean((selectedAgentDetail.personality as Record<string, unknown>).has_identity) && (
                        <span className="text-xs px-2 py-0.5 rounded-full bg-blue-100 dark:bg-blue-900/30 text-blue-700 dark:text-blue-400">{t("AgentsSettings.identity")}</span>
                      )}
                      {Boolean((selectedAgentDetail.personality as Record<string, unknown>).has_memory) && (
                        <span className="text-xs px-2 py-0.5 rounded-full bg-primary-100 dark:bg-primary-900/30 text-primary-700 dark:text-primary-400">{t("AgentsSettings.memory")}</span>
                      )}
                    </div>
                  </div>
                )}
              </div>
            )}

            {/* Runtime config */}
            {selectedAgentDetail?.config && (
              <div>
                <h4 className="text-xs font-semibold text-secondary uppercase tracking-wider mb-2">{t("AgentsSettings.configuration")}</h4>
                <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
                  <div className="px-3 py-2 rounded-lg bg-card">
                    <div className="text-[10px] uppercase tracking-wider text-secondary/70">{t("AgentsSettings.temperature")}</div>
                    <div className="text-sm text-primary">{typeof (selectedAgentDetail.config as Record<string, unknown>).temperature === "number" ? ((selectedAgentDetail.config as Record<string, unknown>).temperature as number).toFixed(2) : "—"}</div>
                  </div>
                  <div className="px-3 py-2 rounded-lg bg-card">
                    <div className="text-[10px] uppercase tracking-wider text-secondary/70">{t("AgentsSettings.maxTokens")}</div>
                    <div className="text-sm text-primary">{String((selectedAgentDetail.config as Record<string, unknown>).max_tokens ?? "—")}</div>
                  </div>
                  <div className="px-3 py-2 rounded-lg bg-card">
                    <div className="text-[10px] uppercase tracking-wider text-secondary/70">{t("AgentsSettings.maxTurns")}</div>
                    <div className="text-sm text-primary">{String((selectedAgentDetail.config as Record<string, unknown>).max_turns ?? "—")}</div>
                  </div>
                  <div className="px-3 py-2 rounded-lg bg-card">
                    <div className="text-[10px] uppercase tracking-wider text-secondary/70">{t("AgentsSettings.maxConcurrentTools")}</div>
                    <div className="text-sm text-primary">{String((selectedAgentDetail.config as Record<string, unknown>).max_concurrent_tools ?? "—")}</div>
                  </div>
                </div>
                {"workspace_only" in (selectedAgentDetail.config as Record<string, unknown>) && (
                  <div className="mt-2 px-3 py-2 rounded-lg bg-card flex items-center justify-between">
                    <span className="text-sm text-secondary">{t("AgentsSettings.workspaceOnly")}</span>
                    <span className={`text-xs px-2 py-0.5 rounded-full ${(selectedAgentDetail.config as Record<string, unknown>).workspace_only ? "bg-primary-100 dark:bg-primary-900/30 text-primary-700 dark:text-primary-400" : "bg-sidebar text-secondary"}`}>
                      {(selectedAgentDetail.config as Record<string, unknown>).workspace_only ? t("AgentsSettings.yes") : t("AgentsSettings.no")}
                    </span>
                  </div>
                )}
              </div>
            )}

            {/* Parameters */}
            <div className="space-y-3">
              <div className="flex items-center justify-between">
                <h4 className="text-xs font-semibold text-secondary uppercase tracking-wider">
                  {isDefault ? t("AgentsSettings.defaultParams") : t("AgentsSettings.namedParams", { name: agentId })}
                </h4>
                {anyOverride && (
                  <button
                    type="button"
                    onClick={() => void resetAgentParams(agentId)}
                    className="text-[11px] text-primary-600 dark:text-primary-400 hover:underline"
                    title={t("AgentsSettings.resetAllTitle")}
                  >
                    {t("AgentsSettings.resetAll")}
                  </button>
                )}
              </div>

              <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
              <div>
                <div className="flex items-center justify-between mb-1">
                  <label className="block text-sm text-secondary">{t("AgentsSettings.temperature")}</label>
                  {overrideHint("temperature")}
                </div>
                <div className="flex items-center gap-2">
                  <input type="range" min="0" max="2" step="0.1" value={eff.temperature} onChange={(e) => void writeField("temperature", parseFloat(e.target.value))} className="flex-1 h-1.5 bg-secondary/20 dark:bg-secondary/20 rounded-lg appearance-none cursor-pointer accent-primary-500" />
                  <span className="text-sm text-secondary w-10 text-right tabular-nums">{eff.temperature.toFixed(2)}</span>
                </div>
              </div>
              <div>
                <div className="flex items-center justify-between mb-1">
                  <label className="block text-sm text-secondary">{t("AgentsSettings.maxTokens")}</label>
                  {overrideHint("max_tokens")}
                </div>
                <Input
                  type="number"
                  value={eff.max_tokens}
                  onChange={(e) => writeNumber("max_tokens", e.target.value)}
                />
              </div>
            </div>
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
              <div>
                <div className="flex items-center justify-between mb-1">
                  <label className="block text-sm text-secondary">{t("AgentsSettings.maxTurns")}</label>
                  {overrideHint("max_turns")}
                </div>
                <Input
                  type="number"
                  value={eff.max_turns ?? ""}
                  placeholder={t("AgentsSettings.unlimited")}
                  onChange={(e) => writeNumber("max_turns", e.target.value, true)}
                />
              </div>
              <div>
                <div className="flex items-center justify-between mb-1">
                  <label className="block text-sm text-secondary">{t("AgentsSettings.maxConcurrentTools")}</label>
                  {overrideHint("max_concurrent_tools")}
                </div>
                <Input
                  type="number"
                  value={eff.max_concurrent_tools}
                  onChange={(e) => writeNumber("max_concurrent_tools", e.target.value)}
                />
              </div>
            </div>
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
              <div>
                <div className="flex items-center justify-between mb-1">
                  <label className="block text-sm text-secondary">{t("AgentsSettings.maxContextTokens")}</label>
                  {overrideHint("max_context_tokens")}
                </div>
                <Input
                  type="number"
                  value={eff.max_context_tokens}
                  onChange={(e) => writeNumber("max_context_tokens", e.target.value)}
                />
              </div>
              <div>
                <div className="flex items-center justify-between mb-1">
                  <label className="block text-sm text-secondary">{t("AgentsSettings.workspaceOnly")}</label>
                  {overrideHint("workspace_only")}
                </div>
                <div className="px-3 py-2 rounded-lg bg-card flex items-center justify-between">
                  <span className="text-xs text-secondary/70">{eff.workspace_only ? t("AgentsSettings.restricted") : t("AgentsSettings.unrestricted")}</span>
                  <Toggle
                    checked={eff.workspace_only}
                    onChange={() => void writeField("workspace_only", !eff.workspace_only)}
                    variant="preset"
                  />
                </div>
              </div>
            </div>
            </div>
          </div>
        )}

        {paramTab === "prompt" && (
          <div>
            <div className="flex items-center justify-between mb-1">
              <label className="block text-sm text-secondary">{t("AgentsSettings.systemPrompt")}</label>
              {overrideHint("system_prompt")}
            </div>
            {isDefault ? (
              <textarea
                value={(da.system_prompt as string | undefined) || ""}
                onChange={(e) => void writeField("system_prompt", e.target.value)}
                className="w-full h-[60vh] rounded-lg border border-subtle bg-card px-3 py-2 text-sm text-primary focus:outline-none focus:ring-2 focus:ring-primary-500/20 resize-none font-mono"
              />
            ) : (
              <>
                <textarea
                  value={ov.system_prompt ?? ""}
                  onChange={(e) =>
                    e.target.value
                      ? void writeField("system_prompt", e.target.value)
                      : void resetAgentParam(agentId, "system_prompt")
                  }
                  placeholder={t("AgentsSettings.promptPlaceholder")}
                  className="w-full h-[60vh] rounded-lg border border-subtle bg-card px-3 py-2 text-sm text-primary focus:outline-none focus:ring-2 focus:ring-primary-500/20 resize-none font-mono"
                />
                <div className="mt-1 text-[11px] text-secondary/70">
                  {t("AgentsSettings.promptHint")}
                </div>
              </>
            )}
          </div>
        )}
      </section>
    </div>
  );
}

