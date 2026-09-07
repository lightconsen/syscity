import { useCallback, useEffect, useState } from "react";
import { Download, Loader2, Lock, RefreshCw, Sparkles, Zap } from "lucide-react";
import { Section } from "@/components/ui/Section";
import { getActiveTransport } from "@/SyscityWebSocketTransport";

interface CatalogEntry {
  id: string;
  version: string;
  display_name: string;
  description: string;
  icon: string | null;
  /** connector | skill | expert */
  type: string;
  /** byoa | cloud */
  kind: string;
  /** public | member */
  visibility: string;
  credits_per_use: number;
  category: string | null;
  installed: boolean;
  installed_version?: string;
  state?: string;
}

interface CatalogResponse {
  version: number;
  synced: boolean;
  entries: CatalogEntry[];
}

const TYPE_LABELS: Record<string, string> = {
  connector: "Connectors",
  skill: "Skills",
  expert: "Experts",
};

/** Fallback glyph per type, used when a catalog entry has no logo. */
const TYPE_ICON: Record<string, string> = {
  connector: "🧩",
  skill: "📦",
  expert: "🧠",
};

const TYPE_ORDER = ["connector", "skill", "expert"];

/** Module-level SWR cache: re-opening the Extensions page (or the settings
 * tab) renders the last catalog instantly while a fresh fetch runs in the
 * background. Mutating actions already call load(), so installed state stays
 * fresh; a full page reload drops it (the gateway round trip is fast). */
let cachedCatalog: CatalogResponse | null = null;

/** Fetch + install + enable cloud/BYOA connectors from the marketplace catalog
 * (P1-4 / P2-8). Cloud entries are metered (`credits_per_use`) and routed
 * through the cloud relay once enabled; BYOA entries are local and free.
 *
 * Experts are summoned, not installed: the Summon button installs the role if
 * needed, then opens a new session bound to the expert agent via
 * `onSummonExpert(agentId)`. */
export function MarketplaceSettings({
  initialType,
  onSummonExpert,
}: {
  /** Pre-select the type filter (connector/skill/expert). */
  initialType?: string;
  onSummonExpert?: (agentId: string) => void;
}) {
  const [data, setData] = useState<CatalogResponse | null>(cachedCatalog);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [typeFilter, setTypeFilter] = useState(initialType ?? "all");

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error("No gateway connection");
      const body = (await transport.getConnectorsCatalog()) as CatalogResponse;
      if ((body as { error?: string }).error) throw new Error((body as { error?: string }).error);
      setData(body);
      cachedCatalog = body;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  /** Install a catalog entry; for experts returns the installed agent id. */
  const install = async (id: string): Promise<string | null> => {
    setBusyId(id);
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error("No gateway connection");
      const body = (await transport.sendRequestAndWait("connectors.catalog_install", {
        id,
      })) as { error?: string; agents?: string[] };
      if (body.error) throw new Error(body.error);
      await load();
      return body.agents?.[0] ?? null;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setBusyId(null);
    }
  };

  /** Summon an expert: install the role if needed, then open a session bound
   * to its agent. */
  const summon = async (e: CatalogEntry) => {
    if (e.installed) {
      onSummonExpert?.(e.id);
      return;
    }
    const agentId = await install(e.id);
    if (agentId) onSummonExpert?.(agentId);
  };

  const setState = async (id: string, action: "enable" | "disable") => {
    setBusyId(`${id}:${action}`);
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error("No gateway connection");
      const body = (await transport.sendRequestAndWait(
        action === "enable" ? "connectors.enable" : "connectors.disable",
        { id }
      )) as { error?: string };
      if (body.error) throw new Error(body.error);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyId(null);
    }
  };

  const entries = (data?.entries ?? []).filter(
    (e) => typeFilter === "all" || e.type === typeFilter,
  );

  return (
    <div className="space-y-5">
      <Section
        title="Extensions"
        right={
          <button
            onClick={load}
            disabled={loading}
            className="inline-flex items-center gap-1 text-xs px-2 py-1 rounded bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
            title="Refresh catalog"
          >
            <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
            Refresh
          </button>
        }
      >
        {/* Type filter */}
        <div className="flex gap-1.5 mb-3 flex-wrap">
          {["all", ...TYPE_ORDER].map((t) => (
            <button
              key={t}
              onClick={() => setTypeFilter(t)}
              className={`px-2.5 py-1 rounded-full text-xs transition ${
                typeFilter === t
                  ? "bg-primary-500 text-white"
                  : "bg-sidebar text-secondary hover:text-primary"
              }`}
            >
              {t === "all" ? "All" : TYPE_LABELS[t] ?? t}
            </button>
          ))}
        </div>

        {error && <p className="text-xs text-red-500 mb-2">{error}</p>}
        {loading && !data ? (
          <div className="flex items-center gap-2 text-secondary text-sm py-6">
            <Loader2 size={14} className="animate-spin" /> Loading catalog...
          </div>
        ) : entries.length === 0 ? (
          <p className="text-sm text-secondary py-4">
            {data && !data.synced
              ? "Catalog is empty. Enable cloud mode and sign in to load extensions."
              : "No entries in this category yet."}
          </p>
        ) : (
          <div className="grid grid-cols-1 sm:grid-cols-2 xl:grid-cols-3 gap-2">
            {entries.map((e) => {
              const busy = busyId === e.id || busyId === `${e.id}:enable` || busyId === `${e.id}:disable`;
              const upToDate = e.installed && e.installed_version === e.version;
              const needsUpdate = e.installed && !upToDate;
              return (
                <div
                  key={`${e.id}@${e.version}`}
                  className="flex flex-col gap-2 px-3 py-2.5 rounded-lg border border-subtle bg-card text-left"
                >
                  <div className="flex items-start gap-2">
                    {e.icon ? (
                      <img src={e.icon} alt="" className="w-5 h-5 object-contain shrink-0 mt-0.5" />
                    ) : (
                      <span className="w-5 h-5 shrink-0 mt-0.5 text-sm">
                        {TYPE_ICON[e.type] ?? "🧩"}
                      </span>
                    )}
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-1.5 flex-wrap">
                        <span className="font-medium text-sm text-primary truncate">{e.display_name}</span>
                        <span className="text-[10px] px-1 py-0.5 rounded bg-sidebar text-secondary">
                          {TYPE_LABELS[e.type] ?? e.type}
                        </span>
                      </div>
                      <p className="text-[11px] leading-tight opacity-70 line-clamp-2 mt-0.5">
                        {e.description || "No description"}
                      </p>
                    </div>
                  </div>

                  <div className="flex items-center gap-1.5 flex-wrap text-[10px]">
                    <span
                      className={`inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded ${
                        e.kind === "cloud"
                          ? "bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-300"
                          : "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-300"
                      }`}
                    >
                      {e.kind === "cloud" ? (
                        <>
                          <Zap size={10} />
                          {e.credits_per_use > 0 ? `${e.credits_per_use} credits/call` : "Cloud"}
                        </>
                      ) : (
                        "Local · free"
                      )}
                    </span>
                    {e.visibility === "member" && (
                      <span className="inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded bg-sidebar text-secondary">
                        <Lock size={10} /> Members
                      </span>
                    )}
                    {e.installed && (
                      <span className="inline-flex items-center px-1.5 py-0.5 rounded bg-sidebar text-secondary">
                        v{e.installed_version}
                      </span>
                    )}
                  </div>

                  <div className="flex items-center gap-1.5 mt-auto">
                    {e.type === "expert" ? (
                      <button
                        onClick={() => summon(e)}
                        disabled={busy}
                        className={`inline-flex items-center gap-1 px-2 py-1 rounded text-[11px] font-medium transition ${
                          e.installed
                            ? "bg-sidebar text-secondary hover:bg-black/5 dark:hover:bg-white/5"
                            : "bg-primary-500 hover:bg-primary-600 text-white"
                        } ${busy ? "opacity-50" : ""}`}
                      >
                        {busy ? <Loader2 size={11} className="animate-spin" /> : <Sparkles size={11} />}
                        Summon
                      </button>
                    ) : (
                      <button
                        onClick={() => install(e.id)}
                        disabled={busy || upToDate}
                        className={`inline-flex items-center gap-1 px-2 py-1 rounded text-[11px] font-medium transition ${
                          upToDate
                            ? "bg-sidebar text-secondary cursor-default"
                            : "bg-primary-500 hover:bg-primary-600 text-white"
                        } ${busy ? "opacity-50" : ""}`}
                      >
                        {busy ? <Loader2 size={11} className="animate-spin" /> : <Download size={11} />}
                        {upToDate ? "Installed" : needsUpdate ? "Update" : "Install"}
                      </button>
                    )}
                    {e.type !== "expert" && e.installed && e.state === "disabled" && (
                      <button
                        onClick={() => setState(e.id, "enable")}
                        disabled={busyId === `${e.id}:enable`}
                        className="px-2 py-1 rounded text-[11px] bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
                      >
                        Enable
                      </button>
                    )}
                    {e.type !== "expert" && e.installed && (e.state === "enabled" || e.state === "installed") && (
                      <button
                        onClick={() => setState(e.id, "disable")}
                        disabled={busyId === `${e.id}:disable`}
                        className="px-2 py-1 rounded text-[11px] bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
                      >
                        Disable
                      </button>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </Section>
    </div>
  );
}
