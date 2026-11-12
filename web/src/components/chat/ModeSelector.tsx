import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, ChevronDown, ShieldCheck, ShieldAlert, ShieldOff, ScanEye } from "lucide-react";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";

type Mode = "default" | "accept_edits" | "plan" | "bypass";

interface ModeSelectorProps {
  transport: SyscityWebSocketTransport;
}

const MODES: Mode[] = ["default", "accept_edits", "plan", "bypass"];

function ModeIcon({ mode, className }: { mode: Mode; className?: string }) {
  switch (mode) {
    case "accept_edits":
      return <ShieldCheck className={className} />;
    case "plan":
      return <ScanEye className={className} />;
    case "bypass":
      return <ShieldOff className={className} />;
    default:
      return <ShieldAlert className={className} />;
  }
}

/**
 * The composer's permission-mode picker.
 *
 * Mirrors `sessions.set_mode` (and the TUI's `/mode`): the four modes are
 * `default` (rules, then hooks, then ask), `accept_edits` (file writers run
 * without asking), `plan` (read-only — everything that writes is refused) and
 * `bypass` (no prompts at all; deny rules and hooks still apply).
 *
 * The override is per session and ephemeral — it lives on the gateway until it
 * restarts, then falls back to `[permissions].mode`. `bypass` additionally
 * needs `[permissions].allow_bypass = true`; the gateway refuses it otherwise
 * and the picker reverts, showing the gateway's own reason.
 */
export function ModeSelector({ transport }: ModeSelectorProps) {
  const { t } = useTranslation("chat");
  const [sessionId, setSessionId] = useState(() => transport.getSessionId());
  const [mode, setMode] = useState<Mode>("default");
  const [allowBypass, setAllowBypass] = useState(false);
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(
    () => transport.onSessionChange(() => setSessionId(transport.getSessionId())),
    [transport]
  );

  // Read the mode the session actually runs in. The gateway resolves the
  // per-session override against the configured default, so this is the truth
  // even before this client has touched anything — and it answers for a
  // conversation the store has never seen, which `sessions.list` cannot.
  useEffect(() => {
    let cancelled = false;
    transport
      .getSessionMode(sessionId)
      .then((res) => {
        if (cancelled || !res) return;
        setMode(res.mode);
        setAllowBypass(res.allowBypass);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [transport, sessionId]);

  // Another client (the TUI, another window) can switch it too.
  useEffect(
    () =>
      transport.onEvent((evt) => {
        if (evt.event !== "session.mode_changed") return;
        const p = evt.payload as { session_id?: string; mode?: string } | undefined;
        if (p?.session_id !== sessionId) return;
        if (p.mode && (MODES as string[]).includes(p.mode)) setMode(p.mode as Mode);
      }),
    [transport, sessionId]
  );

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false);
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  const choose = async (next: Mode) => {
    setOpen(false);
    if (next === mode) return;
    const previous = mode;
    setMode(next); // optimistic; mode_changed and the failure path reconcile
    setError(null);
    const ok = await transport.setSessionMode(sessionId, next);
    if (!ok) {
      setMode(previous);
      setError(t("ModeSelector.rejected"));
    }
  };

  const label = t(`ModeSelector.mode.${mode}`);
  const nonDefault = mode !== "default";

  return (
    <div ref={rootRef} className="relative self-center">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        onMouseDown={(e) => e.preventDefault()}
        title={error ?? t(`ModeSelector.help.${mode}`)}
        aria-label={t("ModeSelector.selectMode")}
        aria-haspopup="listbox"
        aria-expanded={open}
        className={`flex items-center gap-1.5 max-w-[10rem] rounded-lg px-2 py-1.5 text-xs transition focus:outline-none ${
          nonDefault
            ? "text-amber-700 dark:text-amber-400 bg-amber-50 dark:bg-amber-900/20 hover:bg-amber-100 dark:hover:bg-amber-900/30"
            : "text-secondary hover:text-primary hover:bg-black/[0.04] dark:hover:bg-white/[0.06]"
        }`}
      >
        <ModeIcon mode={mode} className="w-3.5 h-3.5 shrink-0" />
        <span className="truncate">{label}</span>
        <ChevronDown
          className={`w-3 h-3 shrink-0 opacity-60 transition-transform ${open ? "rotate-180" : ""}`}
        />
      </button>

      {open && (
        <div
          role="listbox"
          className="absolute bottom-full left-0 mb-1.5 w-max min-w-64 max-w-[min(24rem,calc(100vw-4rem))] bg-card rounded-xl shadow-xl border border-subtle overflow-hidden z-50"
        >
          <div className="py-1">
            {MODES.map((m) => {
              // `bypass` needs `[permissions].allow_bypass`. Showing it as a
              // live choice and then failing on click teaches the user nothing;
              // the gateway told us it is off, so say so here.
              const unavailable = m === "bypass" && !allowBypass;
              return (
                <button
                  key={m}
                  type="button"
                  role="option"
                  aria-selected={m === mode}
                  aria-disabled={unavailable}
                  disabled={unavailable}
                  onClick={() => void choose(m)}
                  title={unavailable ? t("ModeSelector.bypassDisabled") : undefined}
                  className={`w-full flex items-start gap-2 px-3 py-2 text-left transition ${
                    m === mode
                      ? "bg-primary-50 dark:bg-primary-900/20"
                      : unavailable
                        ? "opacity-50 cursor-not-allowed"
                        : "hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
                  }`}
                >
                  <ModeIcon mode={m} className="w-4 h-4 shrink-0 mt-0.5 text-secondary" />
                  <span className="flex-1 min-w-0">
                    <span className="block text-sm text-primary">{t(`ModeSelector.mode.${m}`)}</span>
                    <span className="block text-xs text-secondary">
                      {unavailable ? t("ModeSelector.bypassDisabled") : t(`ModeSelector.help.${m}`)}
                    </span>
                  </span>
                  {m === mode && <Check className="w-4 h-4 text-primary shrink-0 mt-0.5" />}
                </button>
              );
            })}
            <div className="px-3 pt-1.5 pb-2 text-[10px] text-secondary/70">
              {t("ModeSelector.ephemeral")}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
