import { useEffect, useRef, useState } from "react";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { useThemeStore } from "@/stores/themeStore";

// Shared settings-domain types -------------------------------------------------

/** Per-agent parameter overrides. A null/undefined field means "inherit the
 * global default" (default_agent). */
export interface AgentParamOverrides {
  temperature?: number | null;
  max_tokens?: number | null;
  max_turns?: number | null;
  max_concurrent_tools?: number | null;
  workspace_only?: boolean | null;
  system_prompt?: string | null;
  max_context_tokens?: number | null;
}

export interface SyscityConfig {
  model?: string;
  model_provider?: string;
  agent_models?: Record<string, string>;
  default_agent?: {
    temperature?: number;
    max_tokens?: number;
    max_turns?: number | null;
    max_concurrent_tools?: number;
    max_context_tokens?: number;
    system_prompt?: string;
    workspace_only?: boolean;
  };
  agent_overrides?: Record<string, AgentParamOverrides>;
  heartbeat?: {
    enabled?: boolean;
    interval_seconds?: number;
    active_hours_start?: string;
    active_hours_end?: string;
    max_consecutive_idle?: number;
  };
  channels?: ChannelConfig[];
  search?: {
    provider?: string;
    providers?: string[];
    has_api_key?: boolean;
    keys?: Record<string, string>;
  };
}

export interface ChannelConfig {
  name: string;
  channel_type: string;
  enabled: boolean;
  agent_id?: string;
  dm_policy?: string;
  require_mention?: boolean;
  has_credentials?: boolean;
}

export interface EnvField {
  name: string;
  required: boolean;
  description?: string;
}

/** Subset of a preset stored in the env-token modal (no description/logo/enabled). */
export interface EnvPreset {
  name: string;
  display_name: string;
  command?: string;
  args: string[];
  url?: string;
  transport: string;
  auth_type?: string;
  client_id?: string;
  auth_url?: string;
  token_url?: string;
  scopes?: string;
  env: EnvField[];
}

export interface McpPreset extends EnvPreset {
  description: string;
  logo_url?: string;
  enabled: boolean;
}

export interface McpServer {
  id: string;
  transport: string;
  command?: string;
  args: string[];
  url?: string;
  auto_connect: boolean;
  connected: boolean;
}

export interface EnvModalState {
  preset: EnvPreset;
  values: Record<string, string>;
  error?: string;
  saving?: boolean;
}

export interface ToastState {
  message: string;
  type: "success" | "error";
}

// Hook -------------------------------------------------------------------------

export function useSettingsData(
  transport: SyscityWebSocketTransport,
  initialTab = "general",
  /** Agent to select once the registry loads (e.g. from the sidebar). */
  initialAgentId?: string,
) {
  const [config, setConfig] = useState<SyscityConfig>({});
  const [models, setModels] = useState<Array<{ id: string; name: string; provider: string; provider_name: string }>>([]);
  const [agentRegistry, setAgentRegistry] = useState<Array<{ id: string; display_name: string; emoji?: string; is_valid: boolean; has_heartbeat: boolean }>>([]);
  const [crons, setCrons] = useState<Array<Record<string, unknown>>>([]);
  const [skills, setSkills] = useState<Array<Record<string, unknown>>>([]);
  const [loading, setLoading] = useState(false);
  const [activeTab, setActiveTab] = useState(initialTab);
  const currentTheme = useThemeStore((s) => s.theme);
  const [channelActionLoading, setChannelActionLoading] = useState<string>("");
  const [showAddModel, setShowAddModel] = useState(false);
  const [modelActionLoading, setModelActionLoading] = useState<string>("");
  const [mcpServers, setMcpServers] = useState<McpServer[]>([]);
  const [mcpActionLoading, setMcpActionLoading] = useState<string>("");
  const [mcpPresets, setMcpPresets] = useState<McpPreset[]>([]);
  const [authModal, setAuthModal] = useState<{
    serverId: string;
    authUrl: string;
  } | null>(null);
  // Server id whose enable must be rolled back if its pending OAuth
  // authorization fails or is cancelled. Only ever set during the
  // enable-preset flow, so a Cancel/failed auth flips the toggle back off.
  const pendingEnableRevert = useRef<string | null>(null);
  const [envModal, setEnvModal] = useState<EnvModalState | null>(null);
  const [toast, setToast] = useState<ToastState | null>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [selectedAgentId, setSelectedAgentId] = useState("");
  const [selectedAgentDetail, setSelectedAgentDetail] = useState<{
    agent_id: string;
    busy: boolean;
    status: string;
    config: Record<string, unknown> | null;
    personality: Record<string, unknown> | null;
  } | null>(null);
  const [agentDetailLoading, setAgentDetailLoading] = useState(false);

  const loadAgentDetail = async (agentId: string, background = false) => {
    if (!agentId) {
      setSelectedAgentDetail(null);
      return;
    }
    // Background refreshes (after a param edit) keep the current card visible
    // instead of flashing the loading spinner.
    if (!background) setAgentDetailLoading(true);
    try {
      const detail = await transport.getAgent(agentId);
      setSelectedAgentDetail(detail);
    } catch {
      if (!background) setSelectedAgentDetail(null);
    } finally {
      if (!background) setAgentDetailLoading(false);
    }
  };

  useEffect(() => {
    setLoading(true);
    Promise.all([
      transport.getConfig(),
      transport.listModels(),
      transport.listAgentRegistry(),
      transport.listCrons(),
      transport.listSkills(),
      transport.listMcpServers(),
      transport.listMcpPresets(),
    ])
      .then(([cfg, mdl, reg, cronRes, skillRes, mcpRes, mcpPresetRes]) => {
        setConfig(cfg as SyscityConfig);
        setModels(mdl.models || []);
        const registry = reg || [];
        setAgentRegistry(registry);
        setCrons(cronRes.jobs || []);
        setSkills(skillRes.skills || []);
        setMcpServers(mcpRes.servers || []);
        setMcpPresets(mcpPresetRes || []);
        // Auto-select default agent or first available — unless a specific
        // agent was requested (sidebar "Agent settings" on one agent).
        const requested = initialAgentId && registry.some((a) => a.id === initialAgentId);
        const toSelect = requested
          ? (initialAgentId as string)
          : registry.some((a) => a.id === "default")
            ? "default"
            : registry[0]?.id || "";
        if (toSelect) {
          setSelectedAgentId(toSelect);
          loadAgentDetail(toSelect);
        }
      })
      .catch(() => {})
      .finally(() => setLoading(false));
  }, [transport, initialAgentId]);

  // Listen for MCP OAuth events
  useEffect(() => {
    const unsub = transport.onEvent(async (evt) => {
      if (evt.event === "mcp.auth_required") {
        const serverId = evt.payload?.server_id as string;
        const authUrl = evt.payload?.auth_url as string;
        if (serverId && authUrl) {
          setAuthModal({ serverId, authUrl });
        }
      }
      if (evt.event === "mcp.auth_complete") {
        const serverId = evt.payload?.server_id as string;
        setAuthModal(null);
        pendingEnableRevert.current = null;
        // Retry connecting — backend now has a stored token
        if (serverId) {
          const result = await transport.connectMcpServer(serverId);
          if (result.ok) {
            showToast(`Authorization complete, ${serverId} connected`, "success");
          }
        }
        setMcpActionLoading("");
        refreshMcp();
        refreshMcpPresets();
      }
      if (evt.event === "mcp.auth_failed") {
        setAuthModal(null);
        setMcpActionLoading("");
        const serverId = evt.payload?.server_id as string;
        // A failed authorization from the enable-preset flow rolls the enable
        // back so the preset does not stay "Enabled". handleCancelAuth covers
        // the explicit Cancel path; this covers failures that arrive without a
        // Cancel (provider callback / token-exchange errors).
        if (serverId && pendingEnableRevert.current === serverId) {
          pendingEnableRevert.current = null;
          try {
            await transport.removeMcpServer(serverId);
            await transport.disconnectMcpServer(serverId);
          } catch {
            /* rollback is best-effort */
          }
        }
        refreshMcp();
        refreshMcpPresets();
      }
    });
    return unsub;
  }, [transport]);

  const update = async (path: string, value: unknown) => {
    const ok = await transport.setConfig(path, value);
    if (ok) {
      setConfig((prev) => {
        const next = { ...prev };
        const parts = path.split(".");
        if (parts.length === 1) {
          (next as Record<string, unknown>)[parts[0]] = value as never;
        } else if (parts.length === 2 && next[parts[0] as keyof SyscityConfig]) {
          const section = { ...(next[parts[0] as keyof SyscityConfig] as Record<string, unknown>) };
          section[parts[1]] = value;
          (next as Record<string, unknown>)[parts[0]] = section as never;
        } else if (parts.length === 3) {
          // agent_overrides.<agent_id>.<field>
          const section = { ...((next[parts[0] as keyof SyscityConfig] as Record<string, unknown>) ?? {}) };
          const entry = { ...((section[parts[1]] as Record<string, unknown>) ?? {}) };
          if (value === null) {
            delete entry[parts[2]];
          } else {
            entry[parts[2]] = value;
          }
          if (Object.keys(entry).length === 0) {
            delete section[parts[1]];
          } else {
            section[parts[1]] = entry;
          }
          (next as Record<string, unknown>)[parts[0]] = section as never;
        }
        return next;
      });
    }
  };

  /** Selecting an agent loads its detail so the panel shows that agent's
   * parameters instead of the previously selected one. */
  const handleSelectAgent = (id: string) => {
    setSelectedAgentId(id);
    loadAgentDetail(id);
  };

  /** Route a parameter edit to the right config path: the default agent edits
   * `default_agent.*`; named agents write a per-agent override. A null value
   * (or empty string for system_prompt) clears the override. */
  const updateAgentParam = async (agentId: string, field: string, value: unknown) => {
    if (!agentId) return;
    if (agentId === "default") {
      await update(`default_agent.${field}`, value);
    } else {
      await update(`agent_overrides.${agentId}.${field}`, value);
    }
    // Refresh the detail card so it reflects the new effective config.
    await loadAgentDetail(agentId, true);
  };

  /** Clear a single override so the agent inherits the global default. */
  const resetAgentParam = async (agentId: string, field: string) => {
    if (!agentId || agentId === "default") return;
    await update(`agent_overrides.${agentId}.${field}`, null);
    await loadAgentDetail(agentId, true);
  };

  /** Clear every override for an agent. */
  const resetAgentParams = async (agentId: string) => {
    if (!agentId || agentId === "default") return;
    for (const field of [
      "temperature",
      "max_tokens",
      "max_turns",
      "max_concurrent_tools",
      "workspace_only",
      "system_prompt",
      "max_context_tokens",
    ]) {
      await update(`agent_overrides.${agentId}.${field}`, null);
    }
    await loadAgentDetail(agentId, true);
  };

  const da = config.default_agent || {};
  const hb = config.heartbeat || {};
  const channels = config.channels || [];

  const refreshConfig = async () => {
    try {
      const cfg = await transport.getConfig();
      setConfig(cfg as SyscityConfig);
    } catch {
      /* ignore */
    }
  };

  const handleToggleChannel = async (name: string, enabled: boolean) => {
    setChannelActionLoading(name);
    await transport.setChannelEnabled(name, enabled);
    await refreshConfig();
    setChannelActionLoading("");
  };

  const handleRemoveChannel = async (name: string) => {
    if (!confirm(`Remove channel "${name}"?`)) return;
    setChannelActionLoading(name);
    await transport.removeChannel(name);
    await refreshConfig();
    setChannelActionLoading("");
  };

  const handleRemoveModel = async (modelId: string) => {
    if (!confirm(`Remove model "${modelId}"?`)) return;
    setModelActionLoading(modelId);
    await transport.removeModel(modelId);
    await refreshModels();
    setModelActionLoading("");
  };

  const handleSetDefaultModel = async (modelId: string) => {
    setModelActionLoading(`default_${modelId}`);
    const ok = await transport.setDefaultModel(modelId);
    if (ok) {
      await refreshModels();
      setConfig((prev) => ({ ...prev, model: modelId }));
    }
    setModelActionLoading("");
  };

  const refreshModels = async () => {
    try {
      const mdl = await transport.listModels();
      setModels(mdl.models || []);
    } catch {
      /* ignore */
    }
  };

  const refreshMcp = async () => {
    try {
      const res = await transport.listMcpServers();
      setMcpServers(res.servers || []);
    } catch {
      /* ignore */
    }
  };

  const refreshMcpPresets = async () => {
    try {
      const presets = await transport.listMcpPresets();
      setMcpPresets(presets);
    } catch {
      /* ignore */
    }
  };

  const handleEnablePreset = async (preset: McpPreset) => {
    // Presets that need env tokens: collect them first, then validate on save.
    if (preset.env?.length && !preset.enabled) {
      setEnvModal({ preset, values: {} });
      return;
    }
    setMcpActionLoading(preset.name);
    try {
      const res = await transport.addMcpServer({
        id: preset.name,
        transport: preset.transport,
        command: preset.command,
        args: preset.args,
        url: preset.url,
        auth_type: preset.auth_type,
        client_id: preset.client_id,
        auth_url: preset.auth_url,
        token_url: preset.token_url,
        scopes: preset.scopes,
        auto_connect: true,
      });
      if (!res.ok) {
        showToast(`Failed to enable ${preset.display_name}`, "error");
        setMcpActionLoading("");
        return;
      }
      await Promise.all([refreshMcp(), refreshMcpPresets()]);

      // Try connecting — for OAuth servers this may trigger the auth flow
      const result = await transport.connectMcpServer(preset.name);
      if (result.ok) {
        showToast(`${preset.display_name} enabled`, "success");
      } else if (result.errorCode === "MCP_AUTH_REQUIRED" && result.authUrl) {
        // This auth modal came from the enable flow — if it fails or is
        // cancelled, roll the enable back (remove the server config) so the
        // preset toggle returns to disabled.
        pendingEnableRevert.current = preset.name;
        setAuthModal({ serverId: preset.name, authUrl: result.authUrl });
        showToast(`${preset.display_name}: authorization required`, "success");
      } else if (preset.auth_type === "oauth2") {
        // OAuth server but connect returned unexpected error
        showToast(`${preset.display_name}: connect queued, complete auth to connect`, "success");
      } else {
        // For non-OAuth servers, the config was added successfully
        showToast(`${preset.display_name} enabled`, "success");
      }
    } catch {
      showToast(`Failed to enable ${preset.display_name}`, "error");
    }
    setMcpActionLoading("");
  };

  /** Validate + save env tokens for a preset, then enable it. */
  const submitEnv = async () => {
    if (!envModal) return;
    const p = envModal.preset;
    const env: Record<string, string> = {};
    for (const v of p.env) {
      const val = (envModal.values[v.name] ?? "").trim();
      if (v.required && !val) {
        setEnvModal({ ...envModal, error: `${v.name} is required` });
        return;
      }
      if (val) env[v.name] = val;
    }
    setEnvModal({ ...envModal, saving: true, error: undefined });
    const res = await transport.addMcpServer({
      id: p.name,
      transport: p.transport,
      command: p.command,
      args: p.args,
      url: p.url,
      auth_type: p.auth_type,
      client_id: p.client_id,
      auth_url: p.auth_url,
      token_url: p.token_url,
      scopes: p.scopes,
      auto_connect: true,
      env,
    });
    if (!res.ok) {
      // Keep the typed values and the modal open so the user can retry.
      setEnvModal((m) => (m ? { ...m, saving: false, error: res.error || "Failed to enable" } : m));
      return;
    }
    setEnvModal(null);
    await Promise.all([refreshMcp(), refreshMcpPresets()]);
    showToast(`${p.display_name} enabled`, "success");
  };

  const handleCancelAuth = async () => {
    if (authModal) {
      await transport.cancelMcpAuth(authModal.serverId);
      setAuthModal(null);
      setMcpActionLoading("");
      // Roll back the enable: the preset was only marked enabled as part of the
      // now-cancelled authorize flow. Removing the server config flips the
      // preset toggle back to disabled.
      if (pendingEnableRevert.current === authModal.serverId) {
        pendingEnableRevert.current = null;
        try {
          await transport.removeMcpServer(authModal.serverId);
          await transport.disconnectMcpServer(authModal.serverId);
          await Promise.all([refreshMcp(), refreshMcpPresets()]);
        } catch {
          /* rollback is best-effort */
        }
      }
    }
  };

  const handleDisablePreset = async (name: string) => {
    setMcpActionLoading(name);
    try {
      await transport.removeMcpServer(name);
      await transport.disconnectMcpServer(name);
      await Promise.all([refreshMcp(), refreshMcpPresets()]);
      showToast(`${name} disabled`, "success");
    } catch {
      showToast(`Failed to disable ${name}`, "error");
    }
    setMcpActionLoading("");
  };

  const handleRemoveMcp = async (id: string) => {
    if (!confirm(`Remove MCP server "${id}"?`)) return;
    setMcpActionLoading(id);
    await transport.removeMcpServer(id);
    await refreshMcp();
    setMcpActionLoading("");
  };

  const handleConnectMcp = async (id: string) => {
    setMcpActionLoading(id);
    await transport.connectMcpServer(id);
    await refreshMcp();
    setMcpActionLoading("");
  };

  const handleDisconnectMcp = async (id: string) => {
    setMcpActionLoading(id);
    await transport.disconnectMcpServer(id);
    await refreshMcp();
    setMcpActionLoading("");
  };

  const refreshSkills = async () => {
    try {
      const skillRes = await transport.listSkills();
      setSkills(skillRes.skills || []);
    } catch {
      /* ignore */
    }
  };

  const showToast = (message: string, type: "success" | "error") => {
    if (toastTimer.current) clearTimeout(toastTimer.current);
    setToast({ message, type });
    toastTimer.current = setTimeout(() => setToast(null), 3000);
  };

  const tabs = [
    { id: "general", label: "General" },
    { id: "models", label: "Models" },
    { id: "agents", label: "Agents" },
    { id: "channels", label: "Channels" },
    { id: "tools", label: "Web Search" },
    { id: "mcp", label: "MCP Server" },
    { id: "marketplace", label: "Extensions" },
    { id: "skills", label: "Skills" },
    { id: "eval", label: "Eval" },
    { id: "jobs", label: "Jobs" },
    { id: "devices", label: "Devices" },
    { id: "logs", label: "Logs" },
    // All clients: connect to a remote gateway (or run locally on Tauri).
    { id: "connection", label: "Connection" },
  ];

  const tabCls = (id: string) =>
    `px-3 py-1.5 rounded-md text-sm whitespace-nowrap md:w-full md:text-left transition ${
      activeTab === id
        ? "bg-primary-50/70 dark:bg-primary-900/20 text-primary-700 dark:text-primary-400 font-medium"
        : "text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
    }`;

  return {
    config,
    update,
    refreshConfig,
    da,
    hb,
    channels,
    models,
    refreshModels,
    handleRemoveModel,
    handleSetDefaultModel,
    modelActionLoading,
    showAddModel,
    setShowAddModel,
    agentRegistry,
    selectedAgentId,
    setSelectedAgentId,
    handleSelectAgent,
    updateAgentParam,
    resetAgentParam,
    resetAgentParams,
    selectedAgentDetail,
    agentDetailLoading,
    loadAgentDetail,
    crons,
    skills,
    refreshSkills,
    mcpServers,
    mcpPresets,
    refreshMcp,
    refreshMcpPresets,
    handleEnablePreset,
    handleDisablePreset,
    handleConnectMcp,
    handleDisconnectMcp,
    handleRemoveMcp,
    mcpActionLoading,
    channelActionLoading,
    handleToggleChannel,
    handleRemoveChannel,
    authModal,
    setAuthModal,
    envModal,
    setEnvModal,
    submitEnv,
    handleCancelAuth,
    toast,
    showToast,
    loading,
    activeTab,
    setActiveTab,
    tabs,
    tabCls,
    currentTheme,
  };
}
