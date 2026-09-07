import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Section } from "@/components/ui/Section";

/**
 * Connection settings: which gateway this client talks to.
 *
 * - Tauri (desktop/mobile): run the local (embedded/reused) gateway or connect
 *   to a remote gateway on another host. Persisted to `~/.syscity/client.toml`
 *   via Tauri commands and applied on the next app launch.
 * - Browser (web): connect to the serving gateway (same-origin) or to a
 *   configured remote gateway. Persisted to localStorage and applied on reload.
 *
 * In remote mode the gateway must be listening with `auth_mode = "token"` —
 * see docs/remote-access.md.
 */

interface ConnectionConfig {
  mode: "local" | "remote";
  host: string;
  port: number;
  token?: string | null;
}

const inputCls =
  "w-full rounded-lg border border-subtle bg-card px-3 py-1.5 text-sm text-primary focus:outline-none focus:ring-2 focus:ring-primary-500/20";

const isTauri = typeof window !== "undefined" && "__TAURI__" in window;

/** Browser (web) connection settings — localStorage-backed. */
function WebConnectionSettings() {
  const [base, setBase] = useState("");
  const [token, setToken] = useState("");
  const [saving, setSaving] = useState(false);
  const { t } = useTranslation("settings");
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);

  useEffect(() => {
    setBase(localStorage.getItem("syscity_gateway_base") ?? "");
    setToken(localStorage.getItem("syscity_gateway_token") ?? "");
  }, []);

  const save = useCallback(async () => {
    setSaving(true);
    setMsg(null);
    const normalized = base.trim().replace(/\/+$/, "");
    if (normalized) {
      localStorage.setItem("syscity_gateway_base", normalized);
    } else {
      localStorage.removeItem("syscity_gateway_base");
    }
    if (token.trim()) {
      localStorage.setItem("syscity_gateway_token", token.trim());
    } else {
      localStorage.removeItem("syscity_gateway_token");
    }
    setSaving(false);
    setMsg({ ok: true, text: t("ConnectionSettings.savedReloading") });
    window.setTimeout(() => window.location.reload(), 400);
  }, [base, token]);

  return (
    <div className="space-y-5">
      <Section title={t("ConnectionSettings.title")}>
        <p className="text-sm text-secondary mb-3">
          {t("ConnectionSettings.webIntro")}{" "}
          <code className="text-primary">docs/remote-access.md</code>).
        </p>
        <div className="space-y-3 mb-4">
          <div>
            <label className="text-xs text-secondary block mb-1">
              {t("ConnectionSettings.baseUrlLabel")}
            </label>
            <input
              className={inputCls}
              value={base}
              onChange={(e) => setBase(e.target.value)}
              placeholder="http://192.168.1.10:18080"
            />
          </div>
          <div>
            <label className="text-xs text-secondary block mb-1">
              {t("ConnectionSettings.tokenLabel")}
            </label>
            <input
              className={inputCls}
              type="password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
            />
          </div>
        </div>
        {msg && (
          <p
            className={`text-sm mb-3 ${
              msg.ok ? "text-emerald-600 dark:text-emerald-400" : "text-red-600 dark:text-red-400"
            }`}
          >
            {msg.text}
          </p>
        )}
        <button
          onClick={save}
          disabled={saving}
          className="px-3 py-1.5 rounded-lg bg-primary-600 text-white text-sm transition hover:bg-primary-700 disabled:opacity-50"
        >
          {saving ? t("ConnectionSettings.saving") : t("ConnectionSettings.saveReload")}
        </button>
      </Section>
    </div>
  );
}

/** Tauri (desktop/mobile) connection settings — command-backed. */
function TauriConnectionSettings() {
  const { t } = useTranslation("settings");
  const [config, setConfig] = useState<ConnectionConfig | null>(null);
  const [host, setHost] = useState("127.0.0.1");
  const [port, setPort] = useState(18080);
  const [token, setToken] = useState("");
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const { invoke } = await import("@tauri-apps/api/core");
        const c = await invoke<ConnectionConfig>("get_connection");
        if (cancelled) return;
        setConfig(c);
        setHost(c.host);
        setPort(c.port);
        setToken(c.token ?? "");
      } catch {
        // Command unavailable — leave defaults.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const test = useCallback(async () => {
    setTesting(true);
    setMsg(null);
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      const res = await invoke<string>("test_remote_gateway", {
        host,
        port,
        token: token.trim() || null,
      });
      setMsg({ ok: true, text: res });
    } catch (e) {
      setMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setTesting(false);
    }
  }, [host, port, token]);

  const save = useCallback(async () => {
    setSaving(true);
    setMsg(null);
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("save_connection", {
        config: {
          mode: config?.mode ?? "local",
          host,
          port,
          token: token.trim() || null,
        },
      });
      setMsg({ ok: true, text: t("ConnectionSettings.savedRestart") });
    } catch (e) {
      setMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setSaving(false);
    }
  }, [config, host, port, token]);

  if (!config) return null;

  return (
    <div className="space-y-5">
      <Section title={t("ConnectionSettings.title")}>
        <p className="text-sm text-secondary mb-3">
          {t("ConnectionSettings.tauriIntro")}{" "}
          <code className="text-primary">docs/remote-access.md</code>.
        </p>

        <div className="flex items-center gap-4 mb-4">
          <label className="flex items-center gap-2 text-sm">
            <input
              type="radio"
              checked={config.mode === "local"}
              onChange={() => setConfig({ ...config, mode: "local" })}
            />
            {t("ConnectionSettings.localMode")}
          </label>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="radio"
              checked={config.mode === "remote"}
              onChange={() => setConfig({ ...config, mode: "remote" })}
            />
            {t("ConnectionSettings.remoteMode")}
          </label>
        </div>

        {config.mode === "remote" && (
          <div className="space-y-3 mb-4">
            <div>
              <label className="text-xs text-secondary block mb-1">{t("ConnectionSettings.host")}</label>
              <input
                className={inputCls}
                value={host}
                onChange={(e) => setHost(e.target.value)}
                placeholder="192.168.1.10"
              />
            </div>
            <div>
              <label className="text-xs text-secondary block mb-1">{t("ConnectionSettings.port")}</label>
              <input
                className={inputCls}
                type="number"
                value={port}
                onChange={(e) => setPort(Number(e.target.value))}
              />
            </div>
            <div>
              <label className="text-xs text-secondary block mb-1">
                {t("ConnectionSettings.remoteTokenLabel")}
              </label>
              <input
                className={inputCls}
                type="password"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder={t("ConnectionSettings.tokenBlankHint")}
              />
            </div>
            <div className="flex items-center gap-2">
              <button
                onClick={test}
                disabled={testing}
                className="px-3 py-1.5 rounded-lg border border-subtle text-sm transition hover:bg-black/[0.03] dark:hover:bg-white/[0.04] disabled:opacity-50"
              >
                {testing ? t("ConnectionSettings.testing") : t("ConnectionSettings.testConnection")}
              </button>
            </div>
          </div>
        )}

        {msg && (
          <p
            className={`text-sm mb-3 ${
              msg.ok ? "text-emerald-600 dark:text-emerald-400" : "text-red-600 dark:text-red-400"
            }`}
          >
            {msg.text}
          </p>
        )}

        <button
          onClick={save}
          disabled={saving}
          className="px-3 py-1.5 rounded-lg bg-primary-600 text-white text-sm transition hover:bg-primary-700 disabled:opacity-50"
        >
          {saving ? t("ConnectionSettings.saving") : t("ConnectionSettings.save")}
        </button>
      </Section>
    </div>
  );
}

export function ConnectionSettings() {
  return isTauri ? <TauriConnectionSettings /> : <WebConnectionSettings />;
}
