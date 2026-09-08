import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import {
  CalendarCheck,
  Check,
  Coins,
  Copy,
  ExternalLink,
  Gift,
  History,
  Loader2,
  LogOut,
  Receipt,
  Sparkles,
  User,
  Users,
} from "lucide-react";
import {
  cloudClaims,
  cloudDailyClaim,
  cloudInvite,
  cloudLedger,
  cloudLoginUrl,
  cloudLogout,
  cloudPacks,
  cloudRedeemInvite,
  cloudSignupClaim,
  cloudStatus,
  cloudSubscription,
  type CloudClaims,
  type CloudInvite,
  type CloudLedgerEntry,
  type CloudPacks,
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
 *   account menu. The menu is a portal so overflow-x-hidden containers
 *   never clip it. Beyond identity + plan/credits it hosts the earn-credits
 *   surface (C1-C4): daily check-in, signup bonus, invite code, credit
 *   packs, and the recent ledger (D2), each gated by the cloud's
 *   `marketing_enabled` and silently degrading on fetch failures.
 */
export function AccountButton({ variant = "row" }: { variant?: "row" | "icon" }) {
  const { t } = useTranslation("chat");
  const [status, setStatus] = useState<CloudStatus | null>(null);
  const [sub, setSub] = useState<CloudSubscription | null>(null);
  const [claims, setClaims] = useState<CloudClaims | null>(null);
  const [invite, setInvite] = useState<CloudInvite | null>(null);
  const [packs, setPacks] = useState<CloudPacks | null>(null);
  const [ledger, setLedger] = useState<CloudLedgerEntry[] | null>(null);
  const [ledgerOpen, setLedgerOpen] = useState(false);
  const [redeemCode, setRedeemCode] = useState("");
  const [redeemErr, setRedeemErr] = useState<string | null>(null);
  const [redeemBusy, setRedeemBusy] = useState(false);
  const [claimBusy, setClaimBusy] = useState(false);
  const [copied, setCopied] = useState(false);
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

  /** Refresh balance + claim state after a claim/redeem action. */
  const refreshCredits = useCallback(async () => {
    const [s, c, i] = await Promise.all([
      cloudSubscription().catch(() => null),
      cloudClaims().catch(() => null),
      cloudInvite().catch(() => null),
    ]);
    if (s) setSub(s);
    if (c) setClaims(c);
    if (i) setInvite(i);
  }, []);

  /** Fetch the recent ledger lazily (first expand). */
  const loadLedger = useCallback(async () => {
    try {
      const res = await cloudLedger(10);
      setLedger(res.entries ?? []);
    } catch {
      setLedger([]);
    }
  }, []);

  if (!status?.enabled) return null;

  const user = status.user;
  const display = user?.name ?? user?.email ?? user?.id ?? "";
  const initial = display[0]?.toUpperCase();
  const consoleUrl = status.console_url?.replace(/\/$/, "");

  const doDailyClaim = async () => {
    setClaimBusy(true);
    try {
      await cloudDailyClaim();
      await refreshCredits();
    } catch {
      /* silent — state stays stale */
    } finally {
      setClaimBusy(false);
    }
  };

  const doSignupClaim = async () => {
    setClaimBusy(true);
    try {
      await cloudSignupClaim();
      await refreshCredits();
    } catch {
      /* silent */
    } finally {
      setClaimBusy(false);
    }
  };

  const doRedeem = async () => {
    const code = redeemCode.trim();
    if (!code || redeemBusy) return;
    setRedeemBusy(true);
    setRedeemErr(null);
    try {
      await cloudRedeemInvite(code);
      setRedeemCode("");
      await refreshCredits();
    } catch (e) {
      // The cloud returns flat `{"error":"<msg>"}` bodies for 400/404.
      setRedeemErr(e instanceof Error ? e.message : String(e));
    } finally {
      setRedeemBusy(false);
    }
  };

  const copyInvite = async () => {
    if (!invite?.invite_code) return;
    try {
      await navigator.clipboard.writeText(invite.invite_code);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable */
    }
  };

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
    // sits at the right edge, so an unclamped left would push the menu
    // off-screen.
    const MENU_W = 320; // w-80
    const MENU_H = Math.min(560, Math.round(window.innerHeight * 0.7));
    const r = btnRef.current?.getBoundingClientRect();
    if (r) {
      const left = Math.max(8, Math.min(r.left, window.innerWidth - MENU_W - 8));
      let top = r.bottom + 8;
      if (top + MENU_H > window.innerHeight - 8) top = Math.max(8, r.top - MENU_H - 8);
      setMenuPos({ top, left });
    }
    setMenuOpen(true);
    // Refresh everything shown in the menu on open (ledger stays lazy).
    void cloudSubscription()
      .then(setSub)
      .catch(() => {
        /* transient — keep previous value */
      });
    void cloudClaims()
      .then(setClaims)
      .catch(() => setClaims(null));
    void cloudInvite()
      .then(setInvite)
      .catch(() => setInvite(null));
    void cloudPacks()
      .then(setPacks)
      .catch(() => setPacks(null));
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

  const marketing = claims?.marketing_enabled === true;
  const periodEnd = sub?.period_end
    ? new Date(sub.period_end).toLocaleDateString()
    : null;

  // Shared account menu (identical for both variants): identity + plan /
  // credits + earn-credits sections + ledger + console link + sign out.
  const menuEl = (
    <div
      ref={menuRef}
      className="fixed z-50 w-80 rounded-lg border border-subtle bg-card shadow-xl p-2 text-xs flex flex-col"
      style={{ top: menuPos?.top ?? 0, left: menuPos?.left ?? 0 }}
    >
      <div className="max-h-[70vh] overflow-y-auto">
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
          <span className="inline-flex items-center gap-1 text-primary">
            <Coins size={11} className="text-primary-500" />
            {sub ? sub.balance.toLocaleString() : "…"}
          </span>
        </div>
        {periodEnd && (
          <div className="px-2 pb-1 text-secondary/70">
            {t("AccountButton.periodEnd", { date: periodEnd })}
          </div>
        )}

        {marketing && (
          <>
            <div className="my-1 border-t border-subtle" />
            {/* C1: daily check-in */}
            <div className="px-2 py-1.5 flex items-center justify-between gap-2">
              <span className="inline-flex items-center gap-1.5 text-secondary">
                <CalendarCheck size={12} className="text-primary-500" />
                {claims.today_claimed
                  ? t("AccountButton.checkInDone", { streak: claims.streak })
                  : t("AccountButton.checkIn")}
              </span>
              {claims.today_claimed ? null : (
                <button
                  type="button"
                  onClick={doDailyClaim}
                  disabled={claimBusy}
                  className="inline-flex items-center gap-1 px-2 py-1 rounded-md bg-primary-500 hover:bg-primary-600 disabled:opacity-50 text-white font-medium transition"
                >
                  {claimBusy ? (
                    <Loader2 size={11} className="animate-spin" />
                  ) : (
                    <Sparkles size={11} />
                  )}
                  {t("AccountButton.checkInReward", { n: claims.daily_credits })}
                </button>
              )}
            </div>
            <div className="px-2 pb-1 text-secondary/70">
              {t("AccountButton.streakHint", {
                every: claims.streak_bonus_every,
                bonus: claims.streak_bonus_credits,
              })}
            </div>
            {/* C2: signup bonus */}
            {!claims.signup_bonus_claimed && (
              <div className="px-2 py-1.5 flex items-center justify-between gap-2">
                <span className="inline-flex items-center gap-1.5 text-secondary">
                  <Gift size={12} className="text-primary-500" />
                  {t("AccountButton.signupBonus")}
                </span>
                <button
                  type="button"
                  onClick={doSignupClaim}
                  disabled={claimBusy}
                  className="inline-flex items-center gap-1 px-2 py-1 rounded-md bg-primary-500 hover:bg-primary-600 disabled:opacity-50 text-white font-medium transition"
                >
                  {claimBusy ? (
                    <Loader2 size={11} className="animate-spin" />
                  ) : (
                    <Gift size={11} />
                  )}
                  {t("AccountButton.claimSignup", { n: claims.signup_credits })}
                </button>
              </div>
            )}
            {/* C3: invite */}
            <div className="px-2 py-1.5">
              <div className="flex items-center justify-between gap-2">
                <span className="inline-flex items-center gap-1.5 text-secondary">
                  <Users size={12} className="text-primary-500" />
                  {t("AccountButton.inviteTitle")}
                </span>
                <span className="text-secondary/70">
                  {invite
                    ? t("AccountButton.inviteProgress", {
                        n: invite.rewarded_count,
                        limit: invite.reward_limit,
                      })
                    : ""}
                </span>
              </div>
              {invite?.invite_code && (
                <div className="mt-1 flex items-center gap-1">
                  <code className="flex-1 truncate px-2 py-1 rounded bg-sidebar font-mono text-[11px] text-primary">
                    {invite.invite_code}
                  </code>
                  <button
                    type="button"
                    onClick={copyInvite}
                    className="inline-flex items-center gap-1 px-2 py-1 rounded bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition"
                    title={t("AccountButton.inviteCopy")}
                  >
                    {copied ? <Check size={11} /> : <Copy size={11} />}
                    {copied ? t("AccountButton.copied") : t("AccountButton.inviteCopy")}
                  </button>
                </div>
              )}
              <div className="mt-1.5 flex items-center gap-1">
                <input
                  value={redeemCode}
                  onChange={(e) => setRedeemCode(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") void doRedeem();
                  }}
                  placeholder={t("AccountButton.redeemPlaceholder")}
                  className="flex-1 min-w-0 px-2 py-1 rounded border border-subtle bg-sidebar text-[11px] text-primary placeholder:text-secondary/60 focus:outline-none focus:border-primary-500"
                />
                <button
                  type="button"
                  onClick={doRedeem}
                  disabled={redeemBusy || !redeemCode.trim()}
                  className="inline-flex items-center gap-1 px-2 py-1 rounded bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 disabled:opacity-50 text-secondary hover:text-primary transition"
                >
                  {redeemBusy && <Loader2 size={11} className="animate-spin" />}
                  {t("AccountButton.redeem")}
                </button>
              </div>
              {redeemErr && <p className="mt-1 text-[11px] text-red-500">{redeemErr}</p>}
            </div>
            {/* C4: credit packs (purchase stays in the console) */}
            {packs && packs.packs.length > 0 && (
              <div className="px-2 py-1.5">
                <div className="flex items-center gap-1.5 text-secondary mb-1">
                  <Coins size={12} className="text-primary-500" />
                  {t("AccountButton.packsTitle")}
                </div>
                <div className="space-y-1">
                  {packs.packs.map((p) => (
                    <div
                      key={p.id}
                      className="flex items-center justify-between gap-2 px-2 py-1 rounded bg-sidebar"
                    >
                      <span className="text-primary truncate">
                        {p.name} · {p.credits.toLocaleString()}
                      </span>
                      <button
                        type="button"
                        onClick={() => consoleUrl && window.open(consoleUrl, "_blank")}
                        className="shrink-0 px-2 py-0.5 rounded bg-primary-500 hover:bg-primary-600 text-white font-medium transition"
                      >
                        {t("AccountButton.buy")}
                      </button>
                    </div>
                  ))}
                </div>
              </div>
            )}
          </>
        )}

        {/* D2: recent ledger (lazy-loaded, collapsed by default) */}
        <div className="my-1 border-t border-subtle" />
        <div className="px-2 py-1">
          <button
            type="button"
            onClick={() => {
              const next = !ledgerOpen;
              setLedgerOpen(next);
              if (next && ledger === null) void loadLedger();
            }}
            className="w-full flex items-center gap-1.5 text-secondary hover:text-primary transition"
          >
            <History size={12} />
            {t("AccountButton.ledgerTitle")}
          </button>
          {ledgerOpen && (
            <div className="mt-1 space-y-0.5">
              {ledger === null ? (
                <div className="flex items-center gap-1 text-secondary/70">
                  <Loader2 size={10} className="animate-spin" /> …
                </div>
              ) : ledger.length === 0 ? (
                <p className="text-secondary/70">{t("AccountButton.ledgerEmpty")}</p>
              ) : (
                <>
                  {ledger.map((e) => (
                    <div
                      key={e.id}
                      className="flex items-center justify-between gap-2 px-1.5 py-0.5 rounded hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
                    >
                      <span className="truncate text-secondary">
                        {e.reason || e.reference || "—"}
                      </span>
                      <span
                        className={`shrink-0 font-mono ${
                          e.delta >= 0 ? "text-emerald-600 dark:text-emerald-400" : "text-red-500"
                        }`}
                      >
                        {e.delta >= 0 ? "+" : ""}
                        {e.delta.toLocaleString()}
                      </span>
                    </div>
                  ))}
                  {consoleUrl && (
                    <a
                      href={`${consoleUrl}/app/bill`}
                      target="_blank"
                      rel="noreferrer"
                      className="inline-flex items-center gap-1 px-1.5 pt-1 text-secondary hover:text-primary transition"
                    >
                      <Receipt size={11} /> {t("AccountButton.viewAll")}
                    </a>
                  )}
                </>
              )}
            </div>
          )}
        </div>

        <div className="my-1 border-t border-subtle" />
        {consoleUrl && (
          <a
            href={consoleUrl}
            target="_blank"
            rel="noreferrer"
            className="w-full flex items-center gap-2 px-2 py-1.5 rounded-md text-secondary hover:bg-black/5 dark:hover:bg-white/5 hover:text-primary transition"
          >
            <ExternalLink size={12} /> {t("AccountButton.manageConsole")}
          </a>
        )}
      </div>
      <button
        type="button"
        onClick={signOut}
        className="w-full flex items-center gap-2 px-2 py-1.5 rounded-md text-secondary hover:bg-black/5 dark:hover:bg-white/5 hover:text-primary transition shrink-0"
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
