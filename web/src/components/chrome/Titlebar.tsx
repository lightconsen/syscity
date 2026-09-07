import {
  Menu,
  Settings,
  PanelRight,
  ChevronLeft,
  ChevronRight,
} from "lucide-react";
import { useChatStore } from "@/stores/chatStore";
import { usePlatform } from "@/hooks/usePlatform";
import { AccountButton } from "@/components/chat/AccountButton";

interface TitlebarProps {
  isMobile: boolean;
  /** Show the hamburger (mobile drawer trigger). */
  showHamburger: boolean;
  /** Mirror the sidebar width so the left cluster starts at its right edge. */
  sidebarCollapsed: boolean;
  /** Collapse/expand the sidebar (toggle lives in the leading zone). */
  onToggleSidebar: () => void;
  onOpenMobileNav: () => void;
  onOpenSettings: () => void;
  /** Full-screen page opened over the chat area (e.g. Knowledge Base):
   * its title replaces the agent identity in the leading cluster, indented
   * to align with the page's content below. No icons, no close button —
   * leaving the page happens via the sidebar (sessions, New Session). */
  page?: { title: string };
}

const iconBtnCls =
  "p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition";

/**
 * App-wide top bar (hermes-desktop-style shell chrome):
 *
 *   [sidebar-width zone] [agent identity] ...... center drag region ...... [right cluster]
 *
 * - The leading zone mirrors the sidebar's width (w-64 expanded / w-16
 *   collapsed, same 300ms transition) and hosts the sidebar's former
 *   header on md+: logo + "Syscity" + the collapse toggle. The agent
 *   identity strip's left edge therefore tracks the sidebar's right edge
 *   — the titlebar "follows" the pane split below it. On macOS the
 *   native traffic lights overlay this zone (titleBarStyle Overlay);
 *   expanded content clears them with pl-[72px], and when collapsed the
 *   zone is fully covered by the lights, so the toggle moves just after
 *   it (the zone itself stays empty).
 * - tauri-macos: the root carries data-tauri-drag-region="deep" so the whole
 *   bar drags the window (interactive elements are excluded by Tauri's drag
 *   script) and double-click toggles zoom.
 * - every other platform (Windows/Linux desktop, browser, mobile): plain
 *   header strip, native titlebar untouched. Below md the mirror zone
 *   disappears and the hamburger + logo lead the bar instead.
 *
 * Hosts the agent identity strip and the settings/account controls; the
 * network dot and theme toggle live in the Statusbar's sidebar-width zone.
 */
export function Titlebar({
  isMobile,
  showHamburger,
  onOpenMobileNav,
  onOpenSettings,
  sidebarCollapsed,
  onToggleSidebar,
  page,
}: TitlebarProps) {
  const platform = usePlatform();
  const currentAgent = useChatStore((s) => s.currentAgent);
  const workspacePanelOpen = useChatStore((s) => s.workspacePanelOpen);
  const setWorkspacePanelOpen = useChatStore((s) => s.setWorkspacePanelOpen);
  const kbPanelOpen = useChatStore((s) => s.kbPanelOpen);
  const setKbPanelOpen = useChatStore((s) => s.setKbPanelOpen);

  // The "show right sidebar" button manages whichever pane the active page
  // hosts: workspace files / document preview in chat, document preview in
  // the Knowledge Base page.
  const inKbPage = page?.title === "Knowledge Base";
  const rightPanelOpen = inKbPage ? kbPanelOpen : workspacePanelOpen;

  const isMac = platform === "tauri-macos";
  const showSafeAreaTop = platform === "tauri-mobile" || (isMobile && !isMac);

  const collapseBtn = (
    <button
      onClick={onToggleSidebar}
      className="p-1 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition shrink-0"
      title={sidebarCollapsed ? "Expand sidebar" : "Collapse sidebar"}
      aria-label={sidebarCollapsed ? "Expand sidebar" : "Collapse sidebar"}
    >
      {sidebarCollapsed ? (
        <ChevronRight className="w-4 h-4" />
      ) : (
        <ChevronLeft className="w-4 h-4" />
      )}
    </button>
  );

  return (
    <div
      className="h-11 shrink-0 flex items-center bg-page border-b border-subtle pr-2 pl-2 md:pl-0"
      style={
        showSafeAreaTop
          ? { paddingTop: "env(safe-area-inset-top)" }
          : undefined
      }
      data-tauri-drag-region={isMac ? "deep" : undefined}
    >
      {/* Mobile hamburger (drawer trigger) */}
      {showHamburger && (
        <button
          className="md:hidden p-2 -ml-1 rounded-lg text-secondary hover:bg-black/5 dark:hover:bg-white/5 transition"
          onClick={onOpenMobileNav}
          aria-label="Open navigation"
        >
          <Menu size={18} />
        </button>
      )}

      {/* Sidebar-width mirror zone: hosts the sidebar header (logo + name +
          collapse toggle) on md+ and keeps the identity strip flush with the
          sidebar's right edge. Pane-following color: sidebar surface here,
          page surface over the main column, so both columns read as
          full-height panes. The macOS traffic lights overlay this zone;
          when collapsed they fill the whole w-16, so the toggle renders
          just after it instead. */}
      <div
        className={`hidden md:flex shrink-0 h-full items-center bg-rail transition-all duration-300 ${
          sidebarCollapsed ? "w-16" : "w-64"
        } ${sidebarCollapsed ? "justify-center" : isMac ? "pl-[72px]" : "pl-3"}`}
      >
        {!sidebarCollapsed && (
          <>
            <img
              src="/syscity.png"
              alt="Syscity"
              className="w-6 h-6 shrink-0"
              draggable={false}
            />
            <span className="text-sm font-semibold text-primary whitespace-nowrap">
              Syscity
            </span>
            <div className="flex-1" />
            {collapseBtn}
          </>
        )}
        {sidebarCollapsed && !isMac && collapseBtn}
      </div>
      {sidebarCollapsed && isMac && collapseBtn}

      {/* Left cluster — starts exactly at the sidebar's right edge */}
      <div className="flex items-center gap-2 min-w-0">
        {page ? (
          <div className="flex items-center min-w-0 pl-6 md:pl-8">
            <span className="text-sm font-medium text-primary truncate">
              {page.title}
            </span>
          </div>
        ) : (
          <>
            <img
              src="/syscity.png"
              alt="Syscity"
              className="w-5 h-5 shrink-0 md:hidden"
              draggable={false}
            />
            {currentAgent ? (
              <div className="flex items-center gap-2 text-xs text-secondary min-w-0">
                <span className="text-sm shrink-0" aria-hidden="true">
                  {currentAgent.emoji}
                </span>
                <span className="font-medium truncate">
                  {currentAgent.display_name}
                </span>
                {!currentAgent.display_name.includes(currentAgent.id) && (
                  <span className="text-[10px] text-secondary/50 truncate">
                    ({currentAgent.id})
                  </span>
                )}
              </div>
            ) : (
              <span className="md:hidden text-sm font-semibold text-primary whitespace-nowrap">
                Syscity
              </span>
            )}
          </>
        )}
      </div>

      {/* Center: empty drag region */}
      <div className="flex-1 h-full" />

      {/* Right cluster (network dot + theme toggle live in the Statusbar) */}
      <div className="flex items-center gap-1 shrink-0">
        <AccountButton variant="icon" />
        <button onClick={onOpenSettings} className={iconBtnCls} title="Settings" aria-label="Settings">
          <Settings className="w-4 h-4" />
        </button>
        {!isMobile && (
          <button
            type="button"
            title={inKbPage ? "Toggle document panel" : "Browse workspace files"}
            aria-label={inKbPage ? "Toggle document panel" : "Browse workspace files"}
            aria-pressed={rightPanelOpen}
            className={
              rightPanelOpen
                ? "p-1.5 rounded-md text-primary-600 dark:text-primary-400 bg-black/5 dark:bg-white/10 transition"
                : iconBtnCls
            }
            onClick={() =>
              inKbPage
                ? setKbPanelOpen(!kbPanelOpen)
                : setWorkspacePanelOpen(!workspacePanelOpen)
            }
          >
            {/* Hermes-desktop-style "show right sidebar" glyph */}
            <PanelRight className="w-4 h-4" />
          </button>
        )}
      </div>
    </div>
  );
}
