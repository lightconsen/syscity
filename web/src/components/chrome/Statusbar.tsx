import { useTranslation } from "react-i18next";
import { Loader2, Moon, Sun } from "lucide-react";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { useChatStore } from "@/stores/chatStore";
import { useEffectiveModel } from "@/hooks/useEffectiveModel";
import { StatusDot } from "@/components/chat/StatusDot";
import { useThemeStore } from "@/stores/themeStore";

interface StatusbarProps {
  transport: SyscityWebSocketTransport;
  /** Mirror the sidebar width so the status cluster starts at its right edge. */
  sidebarCollapsed: boolean;
}

/**
 * App-wide bottom bar (hermes-desktop-style shell chrome):
 *
 *   [sidebar-width zone: status + theme] ...... [right cluster: runtime/model/version]
 *
 * Pane-following color, mirroring the Titlebar: the leading zone sits on
 * the sidebar surface (it hosts the connection dot + status word and the
 * theme toggle; the word hides when the zone collapses to w-16), the rest
 * sits on the page surface. Right: run indicator + effective model for
 * the active session + gateway version. Context/token usage is
 * intentionally absent — no WS surface exposes it yet (deferred).
 */
export function Statusbar({ transport, sidebarCollapsed }: StatusbarProps) {
  const { t } = useTranslation("chrome");
  const networkStatus = useChatStore((s) => s.networkStatus);
  const isRunning = useChatStore((s) => s.isRunning);
  const { resolvedTheme, setTheme } = useThemeStore();
  // serverInfo is set just before the status flips to "connected", so
  // re-reading it on status change picks up the fresh version.
  const version = networkStatus === "connected" ? transport.getServerInfo().version : undefined;
  const model = useEffectiveModel(transport);
  // A pinned cloud copy is stored as a qualified `cloud/<id>` ref; show the
  // bare model name to the user.
  const modelLabel = model?.replace(/^cloud\//, "");

  return (
    <div
      className="h-7 shrink-0 flex items-center pr-3 bg-page border-t border-subtle text-xs text-secondary"
      style={{ paddingBottom: "env(safe-area-inset-bottom)" }}
    >
      {/* Sidebar-width zone: status + theme on the sidebar surface.
          Auto-width below md (no mirrored pane there). */}
      <div
        className={`flex items-center gap-2 pl-3 pr-2 self-stretch shrink-0 md:transition-all md:duration-300 md:bg-[var(--bg-rail)] ${
          sidebarCollapsed ? "md:w-16" : "md:w-64"
        }`}
      >
        <span className="flex items-center gap-1.5 capitalize shrink-0">
          <StatusDot status={networkStatus} />
          <span className={sidebarCollapsed ? "md:hidden" : undefined}>
            {networkStatus}
          </span>
        </span>
        <button
          onClick={() => setTheme(resolvedTheme === "dark" ? "light" : "dark")}
          className="p-1 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition shrink-0"
          title={t("Statusbar.toggleTheme")}
          aria-label={t("Statusbar.toggleTheme")}
        >
          {resolvedTheme === "dark" ? (
            <Sun className="w-3.5 h-3.5" />
          ) : (
            <Moon className="w-3.5 h-3.5" />
          )}
        </button>
      </div>

      {/* Spacer */}
      <div className="flex-1" />

      {/* Right cluster: runtime / model / gateway version */}
      <div className="flex items-center gap-3 shrink-0">
        {isRunning && (
          <span className="flex items-center gap-1.5" title={t("Statusbar.runningTitle")}>
            <Loader2 className="w-3 h-3 animate-spin" />
            {t("Statusbar.running")}
          </span>
        )}
        {modelLabel && <span className="truncate max-w-[16rem]">{modelLabel}</span>}
        {version && (
          <span className="truncate" title={t("Statusbar.gatewayVersion", { v: version })}>
            v{version}
          </span>
        )}
      </div>
    </div>
  );
}
