import type { SyscityWebSocketTransport } from "../transportCore";

/** `medium` → `Medium`, so the badge table (keyed by the display form) hits. */
function capitaliseRisk(risk: string): string {
  if (!risk) return "Medium";
  return risk.charAt(0).toUpperCase() + risk.slice(1);
}

// Domain mixin: approvals RPC method implementations (installed on the prototype by
// the facade `SyscityWebSocketTransport.ts`). Signatures are merged onto the
// class type in `transportCore.ts`.
export function install(proto: typeof SyscityWebSocketTransport.prototype): void {
  /** `approvals.list` — what is still waiting for a human, from the gateway. */
  proto.listApprovals = async function (
    this: SyscityWebSocketTransport,
  ): Promise<
    Array<{
      approval_id: string;
      tool_name: string;
      requested_by: string;
      risk_level: string;
      message: string;
    }>
  > {
    const res = (await this.sendRequestAndWait("approvals.list", {})) as
      | { approvals?: Array<Record<string, unknown>> }
      | undefined;
    return (res?.approvals ?? []).map((a) => ({
      approval_id: String(a.id ?? a.approval_id ?? ""),
      tool_name: String(a.tool_name ?? ""),
      requested_by: String(a.requested_by ?? ""),
      // The gateway serialises RiskLevel as snake_case; the modal's badge
      // table is keyed by the capitalised display form.
      risk_level: capitaliseRisk(String(a.risk_level ?? "medium")),
      message: String(a.message ?? ""),
    }));
  };

  /**
   * Answer a pending tool approval.
   *
   * `remember` turns the approval into a `[permissions].allow` rule — the
   * "always allow this" the modal offers — which the gateway persists to
   * config. A compound shell command remembers one rule per segment, so the
   * approved chain is reproduced rather than the whole line as typed.
   */
  proto.respondToApproval = async function (
    this: SyscityWebSocketTransport,
    approvalId: string,
    decision: "approve" | "deny",
    reason?: string,
    remember?: boolean,
  ): Promise<boolean> {
    const method = decision === "approve" ? "approvals.approve" : "approvals.deny";
    const payload = decision === "approve"
      ? { id: approvalId, ...(remember ? { remember: true } : {}) }
      : { id: approvalId, reason: reason || "Denied by operator" };
    const res = await this.sendRequestAndWait(method, payload) as { error?: string } | undefined;
    return !res?.error;
  };
}
