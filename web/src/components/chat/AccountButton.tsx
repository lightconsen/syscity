import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { User, LogOut, Loader2 } from "lucide-react";
import {
  cloudLoginUrl,
  cloudLogout,
  cloudStatus,
  cloudSubscription,
  type CloudStatus,
  type CloudSubscription,
} from "@/lib/cloud";

const LOGIN_TIMEOUT_MS = 180_000;
const POLL_MS = 1_500;

/**
 * Account/login entry. Two variants:
 * - "row" (default): full-width sidebar row (like "+ New session").
 * - "icon": compact icon button for the Titlebar right cluster.
 *
 * Sign-in opens the cloud OAuth in a **new tab** (popup flow) so the app
 * never navigates away: the control shows "Signing in…" while the popup
 * runs, the popup notifies back via postMessage and the control also polls
 * `/api/v1/status` as the reliable fallback, flipping to the avatar once the
 * session token is stored. Times out (60s) and resets if the user abandons
 * the popup.
 *
 * - cloud disabled → hidden entirely (matches the rest of the UI).
 * - signed out    → sign-in row/icon.
 * - pending       → spinner row/icon.
 * - signed in     → avatar (+"name" on the row variant) that opens an
 *   account menu (name/email + sign out). The menu is a portal so
 *   overflow-x-hidden containers never clip it, opening below the control.
 */
export function AccountButton({ variant = "row" }: { variant?: "row" | "icon" }) {
  const { t } = useTranslation("chat");
  const [status, setStatus] = useState<CloudStatus | null>(null);
  const [sub, setSub] = useState<CloudSubscription | null>(null);
  const [loginPending, setLoginPending] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const [menuPos, setMenuPos] = useState<{ top: number; left: number } | null>(null);
  const btnRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const pollTimer = useRef<number | null>(null);
  const pendingSince = useRef(0);
  const pendingRef = useRef(false);

  const stopPolling = useCallback(() => {
    if (pollTimer.current !== null) {
      window.clearInterval(pollTimer.current);
      pollTimer.current = null;
    }
  }, []);

  const setPending = useCallback((v: boolean) => {
    pendingRef.current = v;
    setLoginPending(v);
  }, []);

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await cloudStatus());
    } catch {
      /* keep last known status */
    }
  }, []);

  // Initial status (e.g. already signed in from a previous session).
  useEffect(() => {
    refreshStatus();
  }, [refreshStatus]);

  /** Fetch status; stop waiting once logged in (or on timeout). */
  const checkAndMaybeStop = useCallback(async () => {
    try {
      const s = await cloudStatus();
      setStatus(s);
      if (s?.logged_in) {
        stopPolling();
        setPending(false);
      }
    } catch {
      /* transient — keep polling */
    }
    if (pendingRef.current && Date.now() - pendingSince.current > LOGIN_TIMEOUT_MS) {
      stopPolling();
      setPending(false);
    }
  }, [setPending, stopPolling]);

  // Popup → opener notification: the OAuth flow finished, check right away
  // (polling continues as the reliable fallback).
  useEffect(() => {
    const onMessage = (e: MessageEvent) => {
      if (e.data?.type !== "syscity:login") return;
      if (!pendingRef.current) return;
      // The callback may be on localhost while the opener is on 127.0.0.1 —
      // accept either for the local dev loop.
      try {
        const u = new URL(e.origin);
        if (!["localhost", "127.0.0.1"].includes(u.hostname)) return;
        if (u.port !== window.location.port) return;
      } catch {
        return;
      }
      void checkAndMaybeStop();
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [checkAndMaybeStop]);

  const beginLogin = useCallback(() => {
    if (pendingRef.current) return;
    setPending(true);
    pendingSince.current = Date.now();
    window.open(cloudLoginUrl("github"), "_blank");
    stopPolling();
    pollTimer.current = window.setInterval(() => {
      void checkAndMaybeStop();
    }, POLL_MS);
  }, [checkAndMaybeStop, setPending, stopPolling]);

  // Close the menu on outside click.
  useEffect(() => {
    if (!menuOpen) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (btnRef.current?.contains(t) || menuRef.current?.contains(t)) return;
      setMenuOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [menuOpen]);

  if (!status?.enabled) return null;

  const user = status.user;
  const display = user?.name ?? user?.email ?? user?.id ?? "";
  const initial = display[0]?.toUpperCase();

  // Avatar: cloud avatar_url when present, else the initial, else the
  // Profile icon. `extra` layers variant-specific classes (e.g. shrink-0).
  const avatarEl = (extra = "") =>
    user?.avatar_url ? (
      <img
        src={user.avatar_url}
        alt=""
        referrerPolicy="no-referrer"
        className={`w-6 h-6 rounded-full object-cover ${extra}`}
      />
    ) : initial ? (
      <span
        className={`w-6 h-6 rounded-full bg-primary-500 text-white text-xs font-semibold flex items-center justify-center ${extra}`}
      >
        {initial}
      </span>
    ) : (
      <User className={`w-4 h-4 ${extra}`} />
    );

  const toggleMenu = () => {
    if (menuOpen) {
      setMenuOpen(false);
      return;
    }
    // Anchor below the button, clamped to the viewport: the titlebar button
    // sits at the right edge, so an unclamped left would push the value
    // column (plan/credits) off-screen.
    const MENU_W = 224; // w-56
    const MENU_H = 240; // estimated rendered height
    const r = btnRef.current?.getBoundingClientRect();
    if (r) {
      const left = Math.max(8, Math.min(r.left, window.innerWidth - MENU_W - 8));
      let top = r.bottom + 8;
      if (top + MENU_H > window.innerHeight - 8) top = Math.max(8, r.top - MENU_H - 8);
      setMenuPos({ top, left });
    }
    setMenuOpen(true);
    // Fetch plan/balance lazily on first open; refresh each open while the
    // menu is the only place it's shown.
    void cloudSubscription()
      .then(setSub)
      .catch(() => {
        /* transient — keep previous value */
      });
  };

  const rowCls =
    "w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition " +
    "text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04]";
  const iconCls =
    "flex items-center justify-center w-7 h-7 rounded-md transition " +
    "text-secondary hover:bg-black/5 dark:hover:bg-white/5";

  const signOut = () => {
    setMenuOpen(false);
    void cloudLogout();
    setStatus({ ...status, logged_in: false, user: null });
  };

  // Shared account menu (identical for both variants): identity rows +
  // plan/credits + sign out. Rendered through a portal so overflow-x-hidden
  // containers never clip it.
  const menuEl = (
    <div
      ref={menuRef}
      className="fixed z-50 w-56 rounded-lg border border-subtle bg-card shadow-xl p-2 text-xs"
      style={{ top: menuPos?.top ?? 0, left: menuPos?.left ?? 0 }}
    >
      <div className="px-2 py-1.5 flex items-center gap-2">
        {user?.avatar_url ? (
          <img
            src={user.avatar_url}
            alt=""
            referrerPolicy="no-referrer"
            className="w-8 h-8 rounded-full object-cover shrink-0"
          />
        ) : initial ? (
          <span className="w-8 h-8 rounded-full bg-primary-500 text-white text-xs font-semibold flex items-center justify-center shrink-0">
            {initial}
          </span>
        ) : null}
        <div className="text-primary font-medium truncate">
          {display || t("AccountButton.signedIn")}
        </div>
      </div>
      {user?.email && (
        <div className="px-2 pb-1.5 text-secondary truncate">{user.email}</div>
      )}
      {user?.id && (
        <div className="px-2 pb-1.5 text-secondary/70 truncate" title={user.id}>
          {user.id}
        </div>
      )}
      <div className="my-1 border-t border-subtle" />
      <div className="px-2 py-1 flex items-center justify-between gap-2">
        <span className="text-secondary">{t("AccountButton.plan")}</span>
        <span className="text-primary truncate">
          {sub ? t("AccountButton.planName", { plan: sub.plan }) : "…"}
        </span>
      </div>
      <div className="px-2 py-1 flex items-center justify-between gap-2">
        <span className="text-secondary">{t("AccountButton.credits")}</span>
        <span className="text-primary">
          {sub ? sub.balance.toLocaleString() : "…"}
        </span>
      </div>
      <div className="my-1 border-t border-subtle" />
      <button
        type="button"
        onClick={signOut}
        className="w-full flex items-center gap-2 px-2 py-1.5 rounded-md text-secondary hover:bg-black/5 dark:hover:bg-white/5 hover:text-primary transition"
      >
        <LogOut size={12} /> {t("AccountButton.signOut")}
      </button>
    </div>
  );

  if (variant === "icon") {
    if (loginPending) {
      return (
        <button
          disabled
          className={`${iconCls} opacity-70 cursor-default`}
          title={t("AccountButton.signingIn")}
          aria-label={t("AccountButton.signingIn")}
        >
          <Loader2 className="w-4 h-4 animate-spin" />
        </button>
      );
    }
    if (!status.logged_in) {
      return (
        <button
          onClick={beginLogin}
          className={iconCls}
          title={t("AccountButton.signInToCloud")}
          aria-label={t("AccountButton.signInToCloud")}
        >
          <User className="w-4 h-4" />
        </button>
      );
    }
    return (
      <>
        <button
          ref={btnRef}
          onClick={toggleMenu}
          className={iconCls}
          title={display || t("AccountButton.account")}
          aria-label={t("AccountButton.account")}
        >
          {avatarEl()}
        </button>
        {menuOpen &&
          menuPos &&
          createPortal(menuEl, document.body)}
      </>
    );
  }

  if (loginPending) {
    return (
      <button
        disabled
        className={`${rowCls} opacity-70 cursor-default`}
        title={t("AccountButton.signingIn")}
        aria-label={t("AccountButton.signingIn")}
      >
        <Loader2 className="w-4 h-4 shrink-0 animate-spin" />
        <span>{t("AccountButton.signingIn")}</span>
      </button>
    );
  }

  if (!status.logged_in) {
    return (
      <button
        onClick={beginLogin}
        className={rowCls}
        title={t("AccountButton.signInToCloud")}
        aria-label={t("AccountButton.signInToCloud")}
      >
        <User className="w-4 h-4 shrink-0" />
        <span>{t("AccountButton.signIn")}</span>
      </button>
    );
  }

  return (
    <>
      <button
        ref={btnRef}
        onClick={toggleMenu}
        className={`${rowCls} text-primary`}
        title={display || t("AccountButton.account")}
        aria-label={t("AccountButton.account")}
      >
        {avatarEl("shrink-0")}
        <span className="truncate flex-1 text-left">
          {display || t("AccountButton.account")}
        </span>
      </button>
      {menuOpen &&
        menuPos &&
        createPortal(menuEl, document.body)}
    </>
  );
}
