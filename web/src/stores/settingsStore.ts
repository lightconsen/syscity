import { create } from "zustand";

export interface SyscityConfig {
  model?: string;
  model_provider?: string;
  default_agent?: {
    temperature?: number;
    max_tokens?: number;
    max_turns?: number | null;
    max_concurrent_tools?: number;
    system_prompt?: string;
    workspace_only?: boolean;
  };
  heartbeat?: {
    enabled?: boolean;
    interval_seconds?: number;
    active_hours_start?: string;
    active_hours_end?: string;
    max_consecutive_idle?: number;
  };
  channels?: Array<{
    name: string;
    channel_type: string;
    enabled: boolean;
    agent_id?: string;
    dm_policy?: string;
    require_mention?: boolean;
    has_credentials?: boolean;
  }>;
}

export interface ModelItem {
  id: string;
  name: string;
  provider: string;
}

export interface AgentRegistryItem {
  id: string;
  display_name: string;
  is_valid: boolean;
  has_heartbeat: boolean;
  /** True for an agent this connection may use but not reconfigure. */
  read_only?: boolean;
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

export interface ModelPreset {
  name: string;
  display_name: string;
  base_url?: string;
  models: string[];
  protocol?: "open_ai" | "anthropic" | "gemini";
  needs_api_key?: boolean;
}

interface SettingsState {
  config: SyscityConfig;
  models: ModelItem[];
  agentRegistry: AgentRegistryItem[];
  crons: Array<Record<string, unknown>>;
  skills: Array<Record<string, unknown>>;
  mcpServers: McpServer[];
  modelPresets: ModelPreset[];
  loading: boolean;
  activeTab: string;

  setConfig: (config: SyscityConfig) => void;
  setModels: (models: ModelItem[]) => void;
  setAgentRegistry: (registry: AgentRegistryItem[]) => void;
  setCrons: (crons: Array<Record<string, unknown>>) => void;
  setSkills: (skills: Array<Record<string, unknown>>) => void;
  setMcpServers: (servers: McpServer[]) => void;
  setModelPresets: (presets: ModelPreset[]) => void;
  setLoading: (loading: boolean) => void;
  setActiveTab: (tab: string) => void;
}

export const useSettingsStore = create<SettingsState>((set) => ({
  config: {},
  models: [],
  agentRegistry: [],
  crons: [],
  skills: [],
  mcpServers: [],
  modelPresets: [],
  loading: false,
  activeTab: "general",

  setConfig: (config) => set({ config }),
  setModels: (models) => set({ models }),
  setAgentRegistry: (agentRegistry) => set({ agentRegistry }),
  setCrons: (crons) => set({ crons }),
  setSkills: (skills) => set({ skills }),
  setMcpServers: (mcpServers) => set({ mcpServers }),
  setModelPresets: (modelPresets) => set({ modelPresets }),
  setLoading: (loading) => set({ loading }),
  setActiveTab: (activeTab) => set({ activeTab }),
}));
