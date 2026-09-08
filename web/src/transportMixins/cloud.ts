import type { SyscityWebSocketTransport } from "../transportCore";

// Domain mixin: cloud RPC method implementations (installed on the prototype by
// the facade `SyscityWebSocketTransport.ts`). Signatures are merged onto the
// class type in `transportCore.ts`.
export function install(proto: typeof SyscityWebSocketTransport.prototype): void {
  proto.getCloudStatus = async function (this: SyscityWebSocketTransport,): Promise<unknown> {
    return this.sendRequestAndWait("cloud.status", {});
  };
  proto.getCloudSubscription = async function (this: SyscityWebSocketTransport,): Promise<unknown> {
    return this.sendRequestAndWait("cloud.subscription", {});
  };
  proto.getCloudUsage = async function (this: SyscityWebSocketTransport,days = 30): Promise<unknown> {
    return this.sendRequestAndWait("cloud.usage", { days });
  };
  proto.submitCloudToken = async function (this: SyscityWebSocketTransport,token: string): Promise<unknown> {
    return this.sendRequestAndWait("cloud.token", { token });
  };
  proto.cloudLogout = async function (this: SyscityWebSocketTransport,): Promise<void> {
    await this.sendRequestAndWait("cloud.logout", {});
  };
  // Cloud knowledge bases — thin passthroughs to the cloud server. Reads get
  // a relaxed timeout: unlike local RPCs they traverse the WAN to Syscity
  // Cloud, where the 5s default is too tight.
  proto.cloudKbList = async function (this: SyscityWebSocketTransport,): Promise<unknown> {
    return this.sendRequestAndWait("cloud.kb.list", {}, 30_000);
  };
  proto.cloudKbCreate = async function (this: SyscityWebSocketTransport, name: string): Promise<unknown> {
    return this.sendRequestAndWait("cloud.kb.create", { name });
  };
  proto.cloudKbDelete = async function (this: SyscityWebSocketTransport, kbId: string): Promise<{ ok: boolean }> {
    return (await this.sendRequestAndWait("cloud.kb.delete", { kb_id: kbId })) as { ok: boolean };
  };
  // KB backup & sync — the cloud stores local collection snapshots; push/pull
  // walk many documents, so they need far more than the 5 s default timeout.
  proto.cloudKbDocs = async function (this: SyscityWebSocketTransport, kbId: string): Promise<unknown> {
    return this.sendRequestAndWait("cloud.kb.docs", { kb_id: kbId }, 30_000);
  };
  proto.cloudKbPush = async function (this: SyscityWebSocketTransport, collection: string) {
    return (await this.sendRequestAndWait("cloud.kb.push", { collection }, 300_000)) as {
      collection: string;
      cloud_kb_id: string;
      cloud_kb_name: string;
      total: number;
      pushed: number;
      unchanged: number;
      skipped_url: number;
      skipped_external: number;
      too_large: number;
      failed: number;
      errors: string[];
    };
  };
  proto.cloudKbPull = async function (
    this: SyscityWebSocketTransport,
    params: { collection: string } | { cloud_kb_id: string; agent_id: string }
  ) {
    return (await this.sendRequestAndWait("cloud.kb.pull", params, 300_000)) as {
      collection: string;
      agent_id: string;
      cloud_kb_id: string;
      total: number;
      pulled: number;
      unchanged: number;
      failed: number;
      errors: string[];
    };
  };
  // Credits / marketing — thin passthroughs to the cloud server (15s timeout,
  // WAN round trip).
  proto.getCloudClaims = async function (this: SyscityWebSocketTransport): Promise<unknown> {
    return this.sendRequestAndWait("cloud.credits.claims", {}, 15_000);
  };
  proto.cloudDailyClaim = async function (this: SyscityWebSocketTransport): Promise<unknown> {
    return this.sendRequestAndWait("cloud.credits.daily_claim", {}, 15_000);
  };
  proto.cloudSignupClaim = async function (this: SyscityWebSocketTransport): Promise<unknown> {
    return this.sendRequestAndWait("cloud.credits.signup_claim", {}, 15_000);
  };
  proto.getCloudPacks = async function (this: SyscityWebSocketTransport): Promise<unknown> {
    return this.sendRequestAndWait("cloud.credits.packs", {}, 15_000);
  };
  proto.getCloudLedger = async function (
    this: SyscityWebSocketTransport,
    limit?: number
  ): Promise<unknown> {
    return this.sendRequestAndWait(
      "cloud.credits.ledger",
      limit ? { limit } : {},
      15_000
    );
  };
  proto.getCloudInvite = async function (this: SyscityWebSocketTransport): Promise<unknown> {
    return this.sendRequestAndWait("cloud.credits.invite", {}, 15_000);
  };
  proto.redeemCloudInvite = async function (
    this: SyscityWebSocketTransport,
    code: string
  ): Promise<unknown> {
    return this.sendRequestAndWait("cloud.credits.invite_redeem", { code }, 15_000);
  };
}
