import { useCallback, useEffect, useState } from "react";
import { getActiveTransport } from "@/SyscityWebSocketTransport";

/** Latest release info (WS `update.status`). */
export interface UpdateStatus {
  enabled: boolean;
  current: string;
  latest?: string;
  update_available: boolean;
  embedded: boolean;
  /**
   * A background release check is running. `update.status` never waits on
   * GitHub — the WS dispatcher is serial per connection, so awaiting it would
   * stall chat traffic — so a refresh answers with the previous result plus
   * this flag, and the answer is re-read once it clears.
   */
  checking?: boolean;
}

/** Phase/percent (WS `update.progress`). */
export interface UpdateProgress {
  phase:
    | "idle"
    | "checking"
    | "downloading"
    | "verifying"
    | "applying"
    | "restarting"
    | "error";
  percent: number;
  error: string | null;
  current: string;
  latest: string | null;
}

/** True when running inside the Tauri desktop webview. */
export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI__" in window;
}

async function fetchStatus(refresh = false): Promise<UpdateStatus> {
  const transport = getActiveTransport();
  if (!transport) throw new Error("gateway transport not ready");
  return (await transport.getUpdateStatus(refresh)) as UpdateStatus;
}

async function fetchProgress(): Promise<UpdateProgress> {
  const transport = getActiveTransport();
  if (!transport) throw new Error("gateway transport not ready");
  return (await transport.getUpdateProgress()) as UpdateProgress;
}

/**
 * Drives the online-update flow over WS: polls `update.status` on a slow
 * cadence (server-side TTL is 6h); while an update is in flight it polls
 * `update.progress` once a second. In the Tauri desktop webview the trigger
 * button routes to the `check_for_updates` command instead of the daemon
 * (embedded instances refuse binary replacement).
 */
export function useUpdate() {
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  const [progress, setProgress] = useState<UpdateProgress | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await fetchStatus());
    } catch {
      // Gateway unreachable — banner stays hidden until the next poll.
    }
  }, []);

  const refreshProgress = useCallback(async () => {
    try {
      const p = await fetchProgress();
      setProgress(p);
      // The daemon reports idle/error when a run finished; stop polling.
      if (p.phase === "idle" || p.phase === "error") {
        setBusy(false);
        if (p.phase === "error") {
          setError(p.error || "Update failed.");
        }
      }
    } catch {
      // Daemon may be mid-restart; keep polling until it answers again.
    }
  }, []);

  // Slow status poll; also refresh immediately when visibility returns so a
  // freshly-published release appears without a full app reload.
  useEffect(() => {
    refreshStatus();
    const interval = window.setInterval(refreshStatus, 6 * 60 * 1000);
    const onVisible = () => {
      if (document.visibilityState === "visible") refreshStatus();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      window.clearInterval(interval);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [refreshStatus]);

  // 1s progress poll while an update is in flight.
  useEffect(() => {
    if (!busy) return;
    refreshProgress();
    const interval = window.setInterval(refreshProgress, 1000);
    return () => window.clearInterval(interval);
  }, [busy, refreshProgress]);

  // A background release check answers with the previous result; poll until
  // it settles instead of leaving a stale "up to date" up for six minutes.
  const checking = status?.checking === true;
  useEffect(() => {
    if (!checking) return;
    const interval = window.setInterval(refreshStatus, 1000);
    return () => window.clearInterval(interval);
  }, [checking, refreshStatus]);

  const runUpdate = useCallback(async () => {
    setError(null);
    setMessage(null);

    if (isTauri()) {
      // Desktop webview: hand off to the Tauri updater command. The invoke
      // resolves only when no update was found (or errors); when an update is
      // installed the app restarts and the promise never settles.
      try {
        setBusy(true);
        const { invoke } = await import("@tauri-apps/api/core");
        const result = (await invoke("check_for_updates")) as string;
        setBusy(false);
        setMessage(result === "up-to-date" ? "已是最新" : result);
        // The Tauri updater and the gateway's cached status are separate:
        // a check that found nothing still deserves a fresh "current" row,
        // and one that installed an update restarts the app before this runs.
        refreshStatus();
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
        setBusy(false);
      }
      return;
    }

    try {
      const transport = getActiveTransport();
      if (!transport) throw new Error("gateway transport not ready");
      await transport.triggerUpdate();
      setMessage("Downloading update…");
      setBusy(true);
      refreshProgress();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [refreshProgress, refreshStatus]);

  const checkNow = useCallback(async () => {
    setError(null);
    setMessage(null);
    if (isTauri()) {
      await runUpdate();
      return;
    }
    try {
      // `refresh` bypasses the gateway's status cache: "check for updates"
      // after a release was just published must not answer with the cached
      // "up to date" for the next six hours. The check itself runs in the
      // background (the WS dispatcher is serial per connection, so awaiting
      // GitHub would stall chat traffic), so this reply carries the previous
      // result plus `checking`; the effect below polls until it clears.
      setStatus((await fetchStatus(true)) as UpdateStatus);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [runUpdate]);

  return { status, progress, busy, error, message, runUpdate, checkNow };
}
