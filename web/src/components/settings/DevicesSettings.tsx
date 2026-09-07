import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ReactNode } from "react";
import { Camera, MapPin, Bell, Vibrate, FileUp, Wifi } from "lucide-react";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";

interface DeviceCapability {
  id: string;
  label: string;
  available: boolean;
  granted: boolean;
}

interface DevicesSettingsProps {
  transport: SyscityWebSocketTransport;
  showToast: (message: string, type: "success" | "error") => void;
}

export function DevicesSettings({ transport, showToast }: DevicesSettingsProps) {
  const { t } = useTranslation("settings");
  const [deviceCaps, setDeviceCaps] = useState<DeviceCapability[] | null>(null);
  const [deviceCapsLoading, setDeviceCapsLoading] = useState(false);
  const [permRequesting, setPermRequesting] = useState("");
  const [adbPort, setAdbPort] = useState("");
  const [adbConnectPort, setAdbConnectPort] = useState("");
  const [adbCode, setAdbCode] = useState("");
  const [adbStatus, setAdbStatus] = useState<{ paired: boolean; devices: Array<{ serial: string; state: string }> } | null>(null);
  const [adbPairing, setAdbPairing] = useState(false);
  const [adbError, setAdbError] = useState("");
  const [shortcutName, setShortcutName] = useState("");
  const [shortcutInput, setShortcutInput] = useState("");
  const [shortcutRunning, setShortcutRunning] = useState(false);
  const [shortcutMsg, setShortcutMsg] = useState("");
  const [shortcutResults, setShortcutResults] = useState<Array<{ output?: string; at_ms?: number; file?: string }>>([]);
  const [shortcutInbox, setShortcutInbox] = useState<Array<{ prompt?: string; at_ms?: number; file?: string }>>([]);

  // Load device capabilities + adb status when the Devices tab opens
  useEffect(() => {
    if (!transport.isTauri()) return;
    let cancelled = false;
    (async () => {
      setDeviceCapsLoading(true);
      const caps = await transport.deviceCapabilities();
      if (!cancelled) {
        setDeviceCaps(caps);
        setDeviceCapsLoading(false);
      }
      const st = await transport.adbStatus();
      if (!cancelled) setAdbStatus(st);
    })();
    return () => {
      cancelled = true;
    };
  }, [transport]);

  /** Runtime permissions that need the Request button (granted via dialog). */
  const needsDevicePermission = (id: string) =>
    id === "camera" || id === "location" || id === "notifications";

  /** Per-capability icons for the Devices tab list. */
  const deviceCapIcon: Record<string, ReactNode> = {
    camera: <Camera className="w-4 h-4" />,
    location: <MapPin className="w-4 h-4" />,
    notifications: <Bell className="w-4 h-4" />,
    haptics: <Vibrate className="w-4 h-4" />,
    file_pick: <FileUp className="w-4 h-4" />,
    adb: <Wifi className="w-4 h-4" />,
  };

  const requestDevicePermission = async (perm: string) => {
    setPermRequesting(perm);
    const res = await transport.requestDevicePermission(perm);
    setPermRequesting("");
    if (!res) {
      showToast(t("DevicesSettings.errPermRequest", { perm }), "error");
      return;
    }
    // Update the grant state in the list without a full reload.
    setDeviceCaps((caps) =>
      (caps || []).map((c) => (c.id === perm ? { ...c, granted: res.granted } : c))
    );
    showToast(
      res.granted ? t("DevicesSettings.permGranted", { perm }) : t("DevicesSettings.permDenied", { perm }),
      res.granted ? "success" : "error"
    );
  };

  const refreshAdbStatus = async () => {
    const st = await transport.adbStatus();
    setAdbStatus(st);
  };

  const pairAdb = async () => {
    const port = parseInt(adbPort, 10);
    if (!port || !adbCode.trim()) {
      setAdbError(t("DevicesSettings.errPairInput"));
      return;
    }
    setAdbPairing(true);
    setAdbError("");
    const connectPort = adbConnectPort ? parseInt(adbConnectPort, 10) : undefined;
    const res = await transport.adbPair(port, adbCode.trim(), connectPort);
    setAdbPairing(false);
    if (!res) {
      setAdbError(t("DevicesSettings.errPairFailed"));
      return;
    }
    if (res.paired && res.connected) {
      showToast(t("DevicesSettings.pairedToast"), "success");
      setAdbError("");
    } else if (res.paired) {
      setAdbError(res.connectOutput || t("DevicesSettings.errConnectFailed"));
    } else {
      setAdbError(res.pairOutput || t("DevicesSettings.errPairCheckCode"));
    }
    setAdbStatus({ paired: res.connected, devices: res.devices });
  };

  // iOS Shortcuts / AppIntents bus (§4.6)
  const isIOSDevice = /iPhone|iPad|iPod/.test(navigator.userAgent);

  const runShortcut = async () => {
    if (!shortcutName.trim()) {
      setShortcutMsg(t("DevicesSettings.errShortcutName"));
      return;
    }
    setShortcutRunning(true);
    setShortcutMsg("");
    const res = await transport.runShortcut(shortcutName.trim(), shortcutInput || undefined);
    setShortcutRunning(false);
    if (!res) {
      setShortcutMsg(t("DevicesSettings.errShortcutsUnavailable"));
    } else if (res.launched) {
      setShortcutMsg(t("DevicesSettings.shortcutLaunched", { name: shortcutName.trim() }));
    } else {
      setShortcutMsg(t("DevicesSettings.errShortcutLaunch"));
    }
  };

  const refreshShortcutResults = async () => {
    const res = await transport.shortcutResults();
    if (res) setShortcutResults(res);
  };

  const refreshShortcutInbox = async () => {
    const res = await transport.shortcutInbox();
    if (res) setShortcutInbox(res);
  };

  return (
    <div className="space-y-5">
      {!transport.isTauri() || (deviceCaps === null && !deviceCapsLoading) ? (
        <section>
          <div className="rounded-lg bg-card border border-subtle px-4 py-6 text-center text-sm text-secondary">
            {t("DevicesSettings.webOnlyNote")}
          </div>
        </section>
      ) : deviceCapsLoading ? (
        <div className="text-sm text-secondary py-6 text-center">{t("DevicesSettings.loading")}</div>
      ) : (
        <>
          <section>
            <h3 className="text-xs font-semibold text-secondary uppercase tracking-wider mb-2">{t("DevicesSettings.capabilities")}</h3>
            <div className="space-y-2">
              {(deviceCaps || []).map((cap) => (
                <div key={cap.id} className="flex items-center justify-between px-3 py-2 rounded-lg bg-card">
                  <div className="flex items-center gap-2">
                    <span className="text-secondary">{deviceCapIcon[cap.id]}</span>
                    <span className="text-sm text-primary">{cap.label}</span>
                    <span className="text-[10px] text-secondary/70 font-mono">{cap.id}</span>
                  </div>
                  <div className="flex items-center gap-2">
                    <span
                      className={`text-xs px-2 py-0.5 rounded-full ${
                        cap.granted
                          ? "bg-green-500/10 text-green-600 dark:text-green-400"
                          : "bg-amber-500/10 text-amber-600 dark:text-amber-400"
                      }`}
                    >
                      {cap.granted ? t("DevicesSettings.granted") : t("DevicesSettings.notGranted")}
                    </span>
                    {needsDevicePermission(cap.id) && (
                      <button
                        onClick={() => requestDevicePermission(cap.id)}
                        disabled={permRequesting === cap.id}
                        className="px-2.5 py-1 text-xs rounded-md bg-primary-600 text-white hover:opacity-90 transition-opacity disabled:opacity-50"
                      >
                        {permRequesting === cap.id ? t("DevicesSettings.requesting") : t("DevicesSettings.request")}
                      </button>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </section>

          {transport.isTauri() && isIOSDevice && (
            <section>
              <h3 className="text-xs font-semibold text-secondary uppercase tracking-wider mb-2">{t("DevicesSettings.shortcuts")}</h3>
              <div className="rounded-lg bg-card border border-subtle p-3 space-y-3">
                <p className="text-xs text-secondary">
                  {t("DevicesSettings.shortcutsIntro")}
                </p>
                <div className="grid grid-cols-2 gap-2">
                  <div>
                    <label className="block text-xs text-secondary mb-1">{t("DevicesSettings.shortcutNameLabel")}</label>
                    <input
                      placeholder="e.g. Order Coffee"
                      value={shortcutName}
                      onChange={(e) => setShortcutName(e.target.value)}
                      className="w-full rounded-lg border border-subtle bg-card px-3 py-1.5 text-sm text-primary placeholder-gray-400 focus:outline-none focus:ring-2 focus:ring-primary-500/20"
                    />
                  </div>
                  <div>
                    <label className="block text-xs text-secondary mb-1">{t("DevicesSettings.inputLabel")}</label>
                    <input
                      placeholder="Text to pass to the shortcut"
                      value={shortcutInput}
                      onChange={(e) => setShortcutInput(e.target.value)}
                      className="w-full rounded-lg border border-subtle bg-card px-3 py-1.5 text-sm text-primary placeholder-gray-400 focus:outline-none focus:ring-2 focus:ring-primary-500/20"
                    />
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  <button
                    onClick={runShortcut}
                    disabled={shortcutRunning}
                    className="px-3 py-1.5 text-xs font-medium rounded-lg bg-primary-600 text-white hover:opacity-90 transition-opacity disabled:opacity-50"
                  >
                    {shortcutRunning ? t("DevicesSettings.running") : t("DevicesSettings.run")}
                  </button>
                  <button
                    onClick={refreshShortcutResults}
                    className="px-3 py-1.5 text-xs font-medium rounded-lg bg-card border border-subtle text-primary hover:bg-accent/50 transition-colors"
                  >
                    {t("DevicesSettings.fetchOutputs")}
                  </button>
                  <button
                    onClick={refreshShortcutInbox}
                    className="px-3 py-1.5 text-xs font-medium rounded-lg bg-card border border-subtle text-primary hover:bg-accent/50 transition-colors"
                  >
                    {t("DevicesSettings.fetchInbox")}
                  </button>
                </div>
                {shortcutMsg && (
                  <div className="text-xs text-secondary break-words">{shortcutMsg}</div>
                )}
                <div className="grid grid-cols-2 gap-2">
                  <div>
                    <div className="text-[10px] text-secondary/70 uppercase tracking-wider mb-1">{t("DevicesSettings.outputs")}</div>
                    {shortcutResults.length === 0 ? (
                      <div className="text-xs text-secondary">{t("DevicesSettings.nonePending")}</div>
                    ) : (
                      shortcutResults.map((r, i) => (
                        <div key={i} className="text-xs text-primary font-mono break-all bg-accent/30 rounded px-2 py-1 mb-1">
                          {r.output || t("DevicesSettings.noOutput")}
                          {r.at_ms ? <div className="text-[10px] text-secondary/70">{new Date(r.at_ms).toLocaleTimeString()}</div> : null}
                        </div>
                      ))
                    )}
                  </div>
                  <div>
                    <div className="text-[10px] text-secondary/70 uppercase tracking-wider mb-1">{t("DevicesSettings.inbox")}</div>
                    {shortcutInbox.length === 0 ? (
                      <div className="text-xs text-secondary">{t("DevicesSettings.nonePending")}</div>
                    ) : (
                      shortcutInbox.map((p, i) => (
                        <div key={i} className="text-xs text-primary font-mono break-all bg-accent/30 rounded px-2 py-1 mb-1">
                          {p.prompt || t("DevicesSettings.noPrompt")}
                          {p.at_ms ? <div className="text-[10px] text-secondary/70">{new Date(p.at_ms).toLocaleTimeString()}</div> : null}
                        </div>
                      ))
                    )}
                  </div>
                </div>
              </div>
            </section>
          )}

          <section>
            <h3 className="text-xs font-semibold text-secondary uppercase tracking-wider mb-2">{t("DevicesSettings.wirelessDebugging")}</h3>
            <div className="rounded-lg bg-card border border-subtle p-3 space-y-3">
              <p className="text-xs text-secondary">
                {t("DevicesSettings.adbIntro")}
              </p>
              <div className="grid grid-cols-2 gap-2">
                <div>
                  <label className="block text-xs text-secondary mb-1">{t("DevicesSettings.pairingPort")}</label>
                  <input
                    inputMode="numeric"
                    placeholder="e.g. 45678"
                    value={adbPort}
                    onChange={(e) => setAdbPort(e.target.value)}
                    className="w-full rounded-lg border border-subtle bg-card px-3 py-1.5 text-sm text-primary placeholder-gray-400 focus:outline-none focus:ring-2 focus:ring-primary-500/20"
                  />
                </div>
                <div>
                  <label className="block text-xs text-secondary mb-1">{t("DevicesSettings.connectPort")}</label>
                  <input
                    inputMode="numeric"
                    placeholder="e.g. 45679"
                    value={adbConnectPort}
                    onChange={(e) => setAdbConnectPort(e.target.value)}
                    className="w-full rounded-lg border border-subtle bg-card px-3 py-1.5 text-sm text-primary placeholder-gray-400 focus:outline-none focus:ring-2 focus:ring-primary-500/20"
                  />
                </div>
              </div>
              <div>
                <label className="block text-xs text-secondary mb-1">{t("DevicesSettings.pairingCode")}</label>
                <input
                  inputMode="numeric"
                  placeholder="6-digit code"
                  value={adbCode}
                  onChange={(e) => setAdbCode(e.target.value)}
                  className="w-full rounded-lg border border-subtle bg-card px-3 py-1.5 text-sm text-primary placeholder-gray-400 focus:outline-none focus:ring-2 focus:ring-primary-500/20"
                />
              </div>
              <div className="flex items-center gap-2">
                <button
                  onClick={pairAdb}
                  disabled={adbPairing}
                  className="px-3 py-1.5 text-xs font-medium rounded-lg bg-primary-600 text-white hover:opacity-90 transition-opacity disabled:opacity-50"
                >
                  {adbPairing ? t("DevicesSettings.pairing") : t("DevicesSettings.pair")}
                </button>
                <span className="text-xs text-secondary">{t("DevicesSettings.perBoot")}</span>
              </div>
              {adbError && (
                <div className="text-xs text-red-600 dark:text-red-400 break-words">{adbError}</div>
              )}
              <div className="border-t border-subtle pt-2">
                <div className="flex items-center justify-between">
                  <span className="text-xs text-secondary">{t("DevicesSettings.status")}</span>
                  <button onClick={refreshAdbStatus} className="text-xs text-primary-600 hover:underline">
                    {t("DevicesSettings.refresh")}
                  </button>
                </div>
                {adbStatus === null ? (
                  <div className="text-xs text-secondary mt-1">{t("DevicesSettings.unknown")}</div>
                ) : adbStatus.paired ? (
                  <div className="text-xs text-green-600 dark:text-green-400 mt-1">{t("DevicesSettings.statusPaired")}</div>
                ) : (
                  <div className="text-xs text-secondary mt-1">{t("DevicesSettings.statusNotPaired")}</div>
                )}
                {adbStatus && adbStatus.devices.length > 0 && (
                  <div className="mt-1 font-mono text-[11px] text-secondary">
                    {adbStatus.devices.map((d) => `${d.serial} (${d.state})`).join(", ")}
                  </div>
                )}
              </div>
            </div>
          </section>
        </>
      )}
    </div>
  );
}
