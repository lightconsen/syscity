import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Loader2, Lock, Plus, RefreshCw, Search, Sparkles, Zap } from "lucide-react";
import { Section } from "@/components/ui/Section";
import { getActiveTransport } from "@/SyscityWebSocketTransport";
import { pushToast } from "@/components/ui/Toast";

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
  /** Remote-authored starter prompt: pre-fills the composer on summon
   *  (experts) or on the skill toast's "try it" action. */
  starter_prompt?: string | null;
  installed: boolean;
  installed_version?: string;
  state?: string;
  error?: string | null;
  provides_mcp?: boolean;
  connected?: boolean | null;
}

interface CatalogResponse {
  version: number;
  synced: boolean;
  entries: CatalogEntry[];
}

/** Catalog entry type → translation key suffix under `MarketplaceSettings.type*`. */
const TYPE_KEY: Record<string, string> = {
  connector: "typeConnector",
  skill: "typeSkill",
  expert: "typeExpert",
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
 * `onSummonExpert(agentId, starterPrompt?)` — the catalog entry's
 * `starter_prompt` pre-fills the fresh session's composer. Skills toast with
 * a "try it" action (fresh session, pre-filled prompt). Connectors Add =
 * install + connect, with a live connection capsule fed by
 * `connector.<state>` events. */
export function MarketplaceSettings({
  initialType,
  onSummonExpert,
  onNewSessionWithDraft,
}: {
  /** Pre-select the type filter (connector/skill/expert). */
  initialType?: string;
  /** Summon an expert: open a session bound to its agent, with the
   *  catalog entry's starter prompt pre-filled (fresh-session path). */
  onSummonExpert?: (agentId: string, starterPrompt?: string) => void;
  /** Skill follow-up: open a fresh welcome page with a pre-filled composer. */
  onNewSessionWithDraft?: (draft: string) => void;
}) {
  const { t, i18n } = useTranslation("common");
  const [data, setData] = useState<CatalogResponse | null>(cachedCatalog);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  /** Connector Add phase 2 (enable/connect): switches the button label. */
  const [connectingId, setConnectingId] = useState<string | null>(null);
  const [typeFilter, setTypeFilter] = useState(initialType ?? "all");
  const [query, setQuery] = useState("");
  /** Detail modal: track the entry id (not the object) so the modal re-derives
   * installed/state from `data` after every load() and never goes stale. */
  const [detailId, setDetailId] = useState<string | null>(null);

  // Close the detail modal on Escape.
  useEffect(() => {
    if (!detailId) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setDetailId(null);
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [detailId]);

  const load = useCallback(async (refresh = false) => {
    setLoading(true);
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("MarketplaceSettings.noConnection"));
      // The UI language (e.g. "zh-CN") becomes the cloud catalog's
      // Accept-Language on sync — `zh*` resolves the Chinese catalog.
      // `refresh` (Refresh button) forces a server re-fetch past the
      // same-language cache.
      const body = (await transport.getConnectorsCatalog(
        i18n.language,
        refresh
      )) as CatalogResponse;
      if ((body as { error?: string }).error) throw new Error((body as { error?: string }).error);
      setData(body);
      cachedCatalog = body;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [i18n.language, t]);

  useEffect(() => {
    load();
  }, [load]);
  // Reload the catalog when the UI language changes (per-language ETags).

  /** Live connector lifecycle updates (`connector.<state>` events): patch the
   *  visible catalog in place so cards/capsules refresh without a reload, and
   *  toast on new errors (deduped per connector). */
  useEffect(() => {
    const transport = getActiveTransport();
    if (!transport) return;
    const lastErr = new Map<string, string>();
    const patch = (list: CatalogEntry[], id: string, summary?: Record<string, unknown>, state?: string): CatalogEntry[] =>
      list.map((x) =>
        x.id === id
          ? {
              ...x,
              installed: state === "uninstalled" ? false : true,
              installed_version: (summary?.version as string | undefined) ?? x.installed_version,
              state: (summary?.state as string | undefined) ?? state ?? x.state,
              error: (summary?.error as string | undefined) ?? null,
              connected: (summary?.connected as boolean | undefined) ?? x.connected,
            }
          : x,
      );
    const unsub = transport.onEvent((evt) => {
      if (!evt.event.startsWith("connector.")) return;
      const id = evt.payload?.id as string | undefined;
      if (!id) return;
      const summary = evt.payload?.summary as Record<string, unknown> | undefined;
      const state = evt.event.slice("connector.".length);
      setData((prev) => {
        if (!prev) return prev;
        const next = { ...prev, entries: patch(prev.entries, id, summary, state) };
        cachedCatalog = next;
        return next;
      });
      if (evt.event === "connector.error") {
        const msg = (summary?.error as string) ?? id;
        if (lastErr.get(id) !== msg) {
          lastErr.set(id, msg);
          pushToast("error", `${t("MarketplaceSettings.stateError")}: ${msg}`);
        }
      }
    });
    return unsub;
  }, [t]);

  /** Install a catalog entry. Returns the raw response body (the expert
   *  caller reads `agents`, the connector caller reads `state`). */
  const install = async (
    id: string,
  ): Promise<{ type?: string; agents?: string[]; state?: string } | null> => {
    setBusyId(id);
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("MarketplaceSettings.noConnection"));
      const body = (await transport.sendRequestAndWait("connectors.catalog_install", {
        id,
      })) as { error?: string; type?: string; agents?: string[]; state?: string };
      if (body.error) throw new Error(body.error);
      await load();
      return body;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setBusyId(null);
    }
  };

  /** Summon an expert: install the role if needed, then open a session bound
   * to its agent (starter prompt pre-fills the fresh-session composer). */
  const summon = async (e: CatalogEntry) => {
    if (e.installed) {
      onSummonExpert?.(e.id, e.starter_prompt ?? undefined);
      return;
    }
    const res = await install(e.id);
    if (res) onSummonExpert?.(res.agents?.[0] ?? e.id, e.starter_prompt ?? undefined);
  };

  /** Install a skill, then offer a "try it" jump to a fresh session with a
   *  starter prompt referencing the skill. */
  const installSkillEntry = async (e: CatalogEntry) => {
    const res = await install(e.id);
    if (!res) return; // inline error already set
    pushToast(
      "success",
      t("MarketplaceSettings.skillInstalled", { name: e.display_name }),
      {
        label: t("MarketplaceSettings.tryIt"),
        onClick: () =>
          onNewSessionWithDraft?.(
            t("MarketplaceSettings.trySkillPrompt", { name: e.display_name }),
          ),
      },
    );
  };

  /** Connector Add = install + connect: install, then enable so the MCP
   *  connection (local or cloud relay) comes up in one action. */
  const addConnector = async (e: CatalogEntry) => {
    setBusyId(e.id);
    try {
      const res = await install(e.id);
      if (!res) {
        // Install failed: inline error is set (though load() clears it), so
        // make the failure visible with a toast too.
        pushToast("error", t("MarketplaceSettings.connectFailedToast", { name: e.display_name }));
        return;
      }
      if (res.state !== "enabled") {
        // The engine auto-enables auto_connect connectors during install;
        // everything else needs an explicit connect here.
        setConnectingId(e.id);
        const transport = getActiveTransport();
        if (!transport) throw new Error(t("MarketplaceSettings.noConnection"));
        const body = (await transport.sendRequestAndWait("connectors.enable", {
          id: e.id,
        })) as { error?: string };
        if (body.error) throw new Error(body.error);
      }
      pushToast("success", t("MarketplaceSettings.connectedToast", { name: e.display_name }));
    } catch (err) {
      pushToast(
        "error",
        t("MarketplaceSettings.connectFailedToast", { name: e.display_name }),
      );
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setConnectingId(null);
      setBusyId(null);
      await load();
    }
  };

  const setState = async (id: string, action: "enable" | "disable") => {
    setBusyId(`${id}:${action}`);
    setError(null);
    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error(t("MarketplaceSettings.noConnection"));
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

  const q = query.trim().toLowerCase();
  const entries = (data?.entries ?? []).filter(
    (e) =>
      (typeFilter === "all" || e.type === typeFilter) &&
      (!q ||
        e.display_name.toLowerCase().includes(q) ||
        e.id.toLowerCase().includes(q) ||
        e.description.toLowerCase().includes(q)),
  );

  /** Live connection capsule for MCP-backed connectors (cards + modal). */
  const capsule = (e: CatalogEntry) => {
    if (e.type !== "connector" || !e.installed) return null;
    const base = "inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded";
    if (e.state === "error") {
      return (
        <span
          title={e.error ?? undefined}
          className={`${base} bg-red-100 text-red-700 dark:bg-red-900/30 dark:text-red-300`}
        >
          {t("MarketplaceSettings.stateError")}
        </span>
      );
    }
    if (e.state === "enabled") {
      if (e.connected === false) {
        return (
          <span
            title={e.error ?? undefined}
            className={`${base} bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-300`}
          >
            {t("MarketplaceSettings.stateDisconnected")}
          </span>
        );
      }
      return (
        <span
          className={`${base} bg-emerald-100 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-300`}
        >
          {t("MarketplaceSettings.stateConnected")}
        </span>
      );
    }
    if (e.state === "disabled") {
      return (
        <span className={`${base} bg-sidebar text-secondary`}>
          {t("MarketplaceSettings.stateDisabled")}
        </span>
      );
    }
    return null;
  };

  return (
    <div className="space-y-5">
      <Section
        title={t("MarketplaceSettings.title")}
        right={
          <button
            onClick={() => load(true)}
            disabled={loading}
            className="inline-flex items-center gap-1 text-xs px-2 py-1 rounded bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
            title={t("MarketplaceSettings.refreshTitle")}
          >
            <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
            {t("MarketplaceSettings.refresh")}
          </button>
        }
      >
        {/* Type filter + search */}
        <div className="flex gap-2 mb-3 flex-wrap items-center">
          <div className="flex gap-1.5 flex-wrap">
            {["all", ...TYPE_ORDER].map((ty) => (
              <button
                key={ty}
                onClick={() => setTypeFilter(ty)}
                className={`px-2.5 py-1 rounded-full text-xs transition ${
                  typeFilter === ty
                    ? "bg-primary-500 text-white"
                    : "bg-sidebar text-secondary hover:text-primary"
                }`}
              >
                {ty === "all"
                  ? t("MarketplaceSettings.filterAll")
                  : t(`MarketplaceSettings.${TYPE_KEY[ty] ?? ty}`, { defaultValue: ty })}
              </button>
            ))}
          </div>
          <div className="relative w-104">
            <Search
              size={13}
              className="absolute left-2.5 top-1/2 -translate-y-1/2 text-secondary/70 pointer-events-none"
            />
            <input
              type="text"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={t("MarketplaceSettings.searchPlaceholder")}
              className="w-full pl-8 pr-2.5 py-1.5 rounded-lg bg-sidebar border border-subtle text-xs text-primary placeholder:text-secondary/60 focus:outline-none focus:ring-1 focus:ring-primary-400"
            />
          </div>
        </div>

        {error && <p className="text-xs text-red-500 mb-2">{error}</p>}
        {loading && !data ? (
          <div className="flex items-center gap-2 text-secondary text-sm py-6">
            <Loader2 size={14} className="animate-spin" /> {t("MarketplaceSettings.loading")}
          </div>
        ) : entries.length === 0 ? (
          <p className="text-sm text-secondary py-4">
            {data && !data.synced
              ? t("MarketplaceSettings.emptyNotSynced")
              : t("MarketplaceSettings.emptyCategory")}
          </p>
        ) : (
          <div className="grid grid-cols-1 sm:grid-cols-2 xl:grid-cols-3 gap-2">
            {entries.map((e) => {
              const busy = busyId === e.id || busyId === `${e.id}:enable` || busyId === `${e.id}:disable`;
              const connecting = connectingId === e.id;
              const upToDate = e.installed && e.installed_version === e.version;
              const needsUpdate = e.installed && !upToDate;
              const connectorLive = e.type === "connector" && e.state === "enabled";
              const actionLabel = connecting
                ? t("MarketplaceSettings.connecting")
                : upToDate && (e.type !== "connector" || connectorLive)
                  ? t("MarketplaceSettings.installed")
                  : needsUpdate
                    ? t("MarketplaceSettings.update")
                    : t("MarketplaceSettings.add");
              const actionDisabled =
                busy || connecting || (upToDate && (e.type !== "connector" || connectorLive));
              return (
                <div
                  key={`${e.id}@${e.version}`}
                  onClick={() => setDetailId(e.id)}
                  className="flex flex-col gap-2 px-3 py-2.5 rounded-lg border border-subtle bg-card text-left cursor-pointer hover:border-primary-300 transition"
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
                          {t(`MarketplaceSettings.${TYPE_KEY[e.type] ?? e.type}`, { defaultValue: e.type })}
                        </span>
                      </div>
                      <p className="text-[11px] leading-tight opacity-70 line-clamp-2 mt-0.5">
                        {e.description || t("MarketplaceSettings.noDescription")}
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
                          {e.credits_per_use > 0 ? t("MarketplaceSettings.creditsPerCall", { n: e.credits_per_use }) : t("MarketplaceSettings.cloud")}
                        </>
                      ) : (
                        t("MarketplaceSettings.localFree")
                      )}
                    </span>
                    {e.visibility === "member" && (
                      <span className="inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded bg-sidebar text-secondary">
                        <Lock size={10} /> {t("MarketplaceSettings.members")}
                      </span>
                    )}
                    {e.installed && (
                      <span className="inline-flex items-center px-1.5 py-0.5 rounded bg-sidebar text-secondary">
                        v{e.installed_version}
                      </span>
                    )}
                    {capsule(e)}
                  </div>

                  <div className="flex items-center gap-1.5 mt-auto" onClick={(ev) => ev.stopPropagation()}>
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
                        {t("MarketplaceSettings.summon")}
                      </button>
                    ) : (
                      <button
                        onClick={() => (e.type === "skill" ? installSkillEntry(e) : addConnector(e))}
                        disabled={actionDisabled}
                        className={`inline-flex items-center gap-1 px-2 py-1 rounded text-[11px] font-medium transition ${
                          actionDisabled
                            ? "bg-sidebar text-secondary cursor-default"
                            : "bg-primary-500 hover:bg-primary-600 text-white"
                        }`}
                      >
                        {busy || connecting ? <Loader2 size={11} className="animate-spin" /> : <Plus size={11} />}
                        {actionLabel}
                      </button>
                    )}
                    {e.type !== "expert" && e.installed && e.state === "disabled" && (
                      <button
                        onClick={() => setState(e.id, "enable")}
                        disabled={busyId === `${e.id}:enable`}
                        className="px-2 py-1 rounded text-[11px] bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
                      >
                        {t("MarketplaceSettings.enable")}
                      </button>
                    )}
                    {e.type !== "expert" && e.installed && (e.state === "enabled" || e.state === "installed") && (
                      <button
                        onClick={() => setState(e.id, "disable")}
                        disabled={busyId === `${e.id}:disable`}
                        className="px-2 py-1 rounded text-[11px] bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
                      >
                        {t("MarketplaceSettings.disable")}
                      </button>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </Section>

      {/* Entry detail modal. Derived from `data` by id so install/enable/disable
          refreshes are reflected without extra state. */}
      {detailId && (() => {
        const e = (data?.entries ?? []).find((x) => x.id === detailId);
        if (!e) return null;
        const busy = busyId === e.id || busyId === `${e.id}:enable` || busyId === `${e.id}:disable`;
        const connecting = connectingId === e.id;
        const upToDate = e.installed && e.installed_version === e.version;
        const needsUpdate = e.installed && !upToDate;
        const connectorLive = e.type === "connector" && e.state === "enabled";
        const actionLabel = connecting
          ? t("MarketplaceSettings.connecting")
          : upToDate && (e.type !== "connector" || connectorLive)
            ? t("MarketplaceSettings.installed")
            : needsUpdate
              ? t("MarketplaceSettings.update")
              : t("MarketplaceSettings.add");
        const actionDisabled =
          busy || connecting || (upToDate && (e.type !== "connector" || connectorLive));
        return (
          <div
            className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
            onClick={() => setDetailId(null)}
          >
            <div
              className="bg-card rounded-xl p-6 max-w-md w-full mx-4 shadow-xl max-h-[80vh] overflow-y-auto"
              onClick={(ev) => ev.stopPropagation()}
            >
              <div className="flex items-start gap-3">
                {e.icon ? (
                  <img src={e.icon} alt="" className="w-8 h-8 object-contain shrink-0" />
                ) : (
                  <span className="w-8 h-8 shrink-0 text-xl">{TYPE_ICON[e.type] ?? "🧩"}</span>
                )}
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-1.5 flex-wrap">
                    <h3 className="font-semibold text-primary">{e.display_name}</h3>
                    <span className="text-[10px] px-1 py-0.5 rounded bg-sidebar text-secondary">
                      {t(`MarketplaceSettings.${TYPE_KEY[e.type] ?? e.type}`, { defaultValue: e.type })}
                    </span>
                    <span className="text-[10px] px-1 py-0.5 rounded bg-sidebar text-secondary">
                      v{e.version}
                    </span>
                  </div>
                  <p className="text-xs text-secondary mt-1 leading-relaxed whitespace-pre-wrap">
                    {e.description || t("MarketplaceSettings.noDescription")}
                  </p>
                </div>
                <button
                  onClick={() => setDetailId(null)}
                  className="text-secondary hover:text-primary text-sm leading-none px-1 transition"
                  aria-label={t("MarketplaceSettings.close")}
                >
                  ✕
                </button>
              </div>

              <div className="flex items-center gap-1.5 flex-wrap text-[10px] mt-4">
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
                      {e.credits_per_use > 0 ? t("MarketplaceSettings.creditsPerCall", { n: e.credits_per_use }) : t("MarketplaceSettings.cloud")}
                    </>
                  ) : (
                    t("MarketplaceSettings.localFree")
                  )}
                </span>
                {e.visibility === "member" && (
                  <span className="inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded bg-sidebar text-secondary">
                    <Lock size={10} /> {t("MarketplaceSettings.members")}
                  </span>
                )}
                {e.category && (
                  <span className="inline-flex items-center px-1.5 py-0.5 rounded bg-sidebar text-secondary">
                    {e.category}
                  </span>
                )}
                {e.installed && (
                  <span className="inline-flex items-center px-1.5 py-0.5 rounded bg-sidebar text-secondary">
                    {t("MarketplaceSettings.installed")} · v{e.installed_version}
                  </span>
                )}
                {capsule(e)}
                {e.state === "error" && e.error && (
                  <span className="text-red-500 max-w-full truncate" title={e.error}>
                    {e.error}
                  </span>
                )}
              </div>

              <div className="flex items-center gap-1.5 mt-5" onClick={(ev) => ev.stopPropagation()}>
                {e.type === "expert" ? (
                  <button
                    onClick={() => summon(e)}
                    disabled={busy}
                    className={`inline-flex items-center gap-1 px-3 py-1.5 rounded text-xs font-medium transition ${
                      e.installed
                        ? "bg-sidebar text-secondary hover:bg-black/5 dark:hover:bg-white/5"
                        : "bg-primary-500 hover:bg-primary-600 text-white"
                    } ${busy ? "opacity-50" : ""}`}
                  >
                    {busy ? <Loader2 size={12} className="animate-spin" /> : <Sparkles size={12} />}
                    {t("MarketplaceSettings.summon")}
                  </button>
                ) : (
                  <button
                    onClick={() => (e.type === "skill" ? installSkillEntry(e) : addConnector(e))}
                    disabled={actionDisabled}
                    className={`inline-flex items-center gap-1 px-3 py-1.5 rounded text-xs font-medium transition ${
                      actionDisabled
                        ? "bg-sidebar text-secondary cursor-default"
                        : "bg-primary-500 hover:bg-primary-600 text-white"
                    }`}
                  >
                    {busy || connecting ? <Loader2 size={12} className="animate-spin" /> : <Plus size={12} />}
                    {actionLabel}
                  </button>
                )}
                {e.type !== "expert" && e.installed && e.state === "disabled" && (
                  <button
                    onClick={() => setState(e.id, "enable")}
                    disabled={busyId === `${e.id}:enable`}
                    className="px-3 py-1.5 rounded text-xs bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
                  >
                    {t("MarketplaceSettings.enable")}
                  </button>
                )}
                {e.type !== "expert" && e.installed && (e.state === "enabled" || e.state === "installed") && (
                  <button
                    onClick={() => setState(e.id, "disable")}
                    disabled={busyId === `${e.id}:disable`}
                    className="px-3 py-1.5 rounded text-xs bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition"
                  >
                    {t("MarketplaceSettings.disable")}
                  </button>
                )}
              </div>
            </div>
          </div>
        );
      })()}
    </div>
  );
}
