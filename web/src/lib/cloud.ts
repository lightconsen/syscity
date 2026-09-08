// Syscity Cloud session helpers.
//
// These run over WebSocket admin methods (cloud.status, cloud.subscription,
// cloud.usage, cloud.token, cloud.logout) via the active transport — the
// built-in UI is WS-only. cloudLoginUrl is a browser redirect (HTTP) and is
// the one cloud path that always uses HTTP.

import { getGatewayBase } from "./gatewayBase";
import { getActiveTransport } from "@/SyscityWebSocketTransport";

export interface CloudStatus {
  enabled: boolean;
  logged_in: boolean;
  user: {
    id?: string;
    name?: string;
    email?: string | null;
    avatar_url?: string | null;
  } | null;
  /** Cloud console web app root (billing / earn-credits deep links). */
  console_url?: string;
}

export interface CloudSubscription {
  plan: string;
  plan_rank: number;
  status: string;
  balance: number;
  overdrawn: boolean;
  threshold_warn: boolean;
  period_end: string | null;
}

export interface CloudUsage {
  days: number;
  month_credits: number;
  total_calls: number;
  total_credits: number;
  by_model: Array<{ model: string; calls: number; credits: number }>;
  by_category: Array<{ category: string; calls: number; credits: number }>;
}

/** GET credits/claims — marketing state (`marketing_enabled` is the master
 * switch for the check-in / signup / invite UI). All values are supplied by
 * the cloud; the client never hardcodes promotion numbers. */
export interface CloudClaims {
  marketing_enabled: boolean;
  today_claimed: boolean;
  streak: number;
  signup_bonus_claimed: boolean;
  signup_credits: number;
  daily_credits: number;
  streak_bonus_credits: number;
  streak_bonus_every: number;
}

export interface DailyClaimResult {
  claimed: boolean;
  streak: number;
  daily: number;
  bonus: number;
  balance: number;
}

export interface SignupClaimResult {
  claimed: boolean;
  bonus: number;
  balance: number;
}

/** GET credits/packs — purchasable credit packs (price ascending). */
export interface CloudPacks {
  packs: Array<{
    id: string;
    name: string;
    credits: number;
    price_cents: number;
    price_usd_cents: number;
  }>;
}

/** One ledger entry (credits granted/spent, newest first). */
export interface CloudLedgerEntry {
  id: string;
  delta: number;
  reason: string;
  reference?: string | null;
  model?: string | null;
  expires_at?: string | null;
  created_at: string;
}

export interface CloudInvite {
  invite_code: string | null;
  invited_by?: string | null;
  invitee_count: number;
  rewarded_count: number;
  reward_limit: number;
  bonus_credits: number;
  pending_rewards?: { activation?: boolean; upgrade?: boolean };
  balance: number;
}

export interface RedeemResult {
  redeemed: boolean;
  bonus: number;
  balance: number;
}

/** Cloud status — `null` in default (non-cloud) builds. */
export async function cloudStatus(): Promise<CloudStatus> {
  const transport = getActiveTransport();
  if (!transport) throw new Error("No gateway connection");
  const body = (await transport.getCloudStatus()) as CloudStatus | null;
  if (!body) return { enabled: false, logged_in: false, user: null };
  return body;
}

/** Plan + credit balance (+ low-credit/overdraft flags). */
export async function cloudSubscription(): Promise<CloudSubscription> {
  const transport = getActiveTransport();
  if (!transport) throw new Error("No gateway connection");
  return (await transport.getCloudSubscription()) as CloudSubscription;
}

/** Credit usage for the last `days` (default 30). */
export async function cloudUsage(days = 30): Promise<CloudUsage> {
  const transport = getActiveTransport();
  if (!transport) throw new Error("No gateway connection");
  return (await transport.getCloudUsage(days)) as CloudUsage;
}

/** The cloud OAuth login URL (engine route that 302s to the cloud). */
export function cloudLoginUrl(provider = "github"): string {
  return `${getGatewayBase()}/api/v1/login?provider=${provider}`;
}

/** Redirect the current tab to the cloud OAuth login (welcome-page flow). */
export function cloudLogin(provider = "github") {
  window.location.href = cloudLoginUrl(provider);
}

/** Persist a session token returned by the cloud OAuth callback. */
export async function cloudSubmitToken(token: string): Promise<boolean> {
  const transport = getActiveTransport();
  if (!transport) return false;
  // The OAuth callback page submits on mount, racing the transport's WS
  // handshake; sendRequestAndWait fails fast while the socket is still
  // CONNECTING. Wait for "connected" first (onStatusChange fires immediately
  // with the current status, so this also passes through an open socket).
  const ready = await new Promise<boolean>((resolve) => {
    const timer = setTimeout(() => {
      unsub();
      resolve(false);
    }, 8000);
    const unsub = transport.onStatusChange((status) => {
      if (status === "connected") {
        clearTimeout(timer);
        unsub();
        resolve(true);
      }
    });
  });
  if (!ready) return false;
  try {
    const r = (await transport.submitCloudToken(token)) as { ok?: boolean };
    return r?.ok ?? true;
  } catch {
    return false;
  }
}

/** Forget the cloud session token. */
export async function cloudLogout(): Promise<void> {
  const transport = getActiveTransport();
  if (!transport) return;
  await transport.cloudLogout();
}

// --- Credits / marketing (cloud.credits.*) ---

async function requireTransport() {
  const transport = getActiveTransport();
  if (!transport) throw new Error("No gateway connection");
  return transport;
}

/** Marketing/check-in state for the account. */
export async function cloudClaims(): Promise<CloudClaims> {
  return (await (await requireTransport()).getCloudClaims()) as CloudClaims;
}

/** The daily check-in (`claimed:false` when already claimed / marketing off). */
export async function cloudDailyClaim(): Promise<DailyClaimResult> {
  return (await (await requireTransport()).cloudDailyClaim()) as DailyClaimResult;
}

/** The one-time signup bonus. */
export async function cloudSignupClaim(): Promise<SignupClaimResult> {
  return (await (await requireTransport()).cloudSignupClaim()) as SignupClaimResult;
}

/** Purchasable credit packs. */
export async function cloudPacks(): Promise<CloudPacks> {
  return (await (await requireTransport()).getCloudPacks()) as CloudPacks;
}

/** Recent credit ledger entries (default 50, newest first). */
export async function cloudLedger(limit?: number): Promise<{ entries: CloudLedgerEntry[] }> {
  return (await (await requireTransport()).getCloudLedger(limit)) as {
    entries: CloudLedgerEntry[];
  };
}

/** The account's invite code + reward progress. */
export async function cloudInvite(): Promise<CloudInvite> {
  return (await (await requireTransport()).getCloudInvite()) as CloudInvite;
}

/** Redeem someone else's invite code (throws the cloud's error message). */
export async function cloudRedeemInvite(code: string): Promise<RedeemResult> {
  return (await (await requireTransport()).redeemCloudInvite(code)) as RedeemResult;
}
