import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import type { SyscityConfig } from "@/components/settings/useSettingsData";
import { Section } from "@/components/ui/Section";

const SEARCH_PROVIDERS = [
  { id: "duckduckgo", label: "DuckDuckGo", needsKey: false },
  { id: "tavily", label: "Tavily", needsKey: true },
  { id: "serpapi", label: "SerpAPI", needsKey: true },
  { id: "exa", label: "Exa", needsKey: true },
  { id: "firecrawl", label: "Firecrawl", needsKey: true },
  { id: "serper", label: "Serper", needsKey: true },
  { id: "bocha", label: "Bocha", needsKey: true },
  { id: "brave", label: "Brave", needsKey: true },
];

// Provider-key rows use a separate input style (inline label, no focus ring).
const KEY_INPUT_CLASS = "flex-1 text-sm border border-subtle rounded-lg px-3 py-2 bg-card text-primary placeholder-gray-400";

interface ToolsSettingsProps {
  config: SyscityConfig;
  update: (path: string, value: unknown) => Promise<void>;
}

export function ToolsSettings({ config, update }: ToolsSettingsProps) {
  const { t } = useTranslation("settings");
  return (
    <div className="space-y-5">
      {/* Default Provider */}
      <Section title={t("ToolsSettings.defaultProvider")}>
        <select
          value={config.search?.provider ?? "duckduckgo"}
          onChange={(e) => update("search.provider", e.target.value)}
          className="w-full text-sm border border-subtle rounded-lg px-3 py-2 bg-card text-primary"
        >
          {SEARCH_PROVIDERS.map((p) => (
            <option key={p.id} value={p.id}>{p.label}</option>
          ))}
        </select>
      </Section>

      {/* Fallback Provider Order */}
      <Section title={t("ToolsSettings.fallbackOrder")}>
        <div className="flex flex-wrap gap-2 mb-2">
          {(config.search?.providers ?? []).map((prov) => (
            <span key={prov} className="inline-flex items-center gap-1 px-2.5 py-1 rounded-full text-xs bg-sidebar text-secondary">
              {prov}
              <button
                onClick={() => {
                  const updated = (config.search?.providers ?? []).filter((p) => p !== prov);
                  update("search.providers", updated);
                }}
                className="hover:text-red-500 transition"
              >
                <X size={12} />
              </button>
            </span>
          ))}
        </div>
        <div className="flex gap-2">
          <select
            value=""
            onChange={(e) => {
              const val = e.target.value;
              if (val) {
                const current = config.search?.providers ?? [];
                if (!current.includes(val)) {
                  update("search.providers", [...current, val]);
                }
              }
            }}
            className="text-sm border border-subtle rounded-lg px-3 py-2 bg-card text-primary"
          >
            <option value="">{t("ToolsSettings.addProvider")}</option>
            {SEARCH_PROVIDERS.filter((p) => !(config.search?.providers ?? []).includes(p.id)).map((p) => (
              <option key={p.id} value={p.id}>{p.label}</option>
            ))}
          </select>
        </div>
      </Section>

      {/* API Keys */}
      <Section title={t("ToolsSettings.apiKeys")}>
        <div className="space-y-3">
          {SEARCH_PROVIDERS.filter((p) => p.needsKey).map((p) => (
            <div key={p.id} className="flex items-center gap-3">
              <label className="w-28 text-sm text-secondary shrink-0">{p.label}</label>
              <input
                type="password"
                placeholder={config.search?.keys?.[p.id] === "true" ? "••••••••" : ""}
                value=""
                onChange={(e) => update(`search.keys.${p.id}`, e.target.value)}
                onFocus={(e) => (e.target.value = "")}
                className={KEY_INPUT_CLASS}
              />
            </div>
          ))}
        </div>
      </Section>
    </div>
  );
}
