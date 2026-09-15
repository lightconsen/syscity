import type { SyscityWebSocketTransport } from "../transportCore";

// Domain mixin: agents RPC method implementations (installed on the prototype by
// the facade `SyscityWebSocketTransport.ts`). Signatures are merged onto the
// class type in `transportCore.ts`.
export function install(proto: typeof SyscityWebSocketTransport.prototype): void {
  proto.listAgents = async function (this: SyscityWebSocketTransport,): Promise<{ agents: string[] }> {
    const res = await this.sendRequestAndWait("agents.list", {}) as { agents: string[] } | undefined;
    return res || { agents: [] };
  };
  proto.getAgent = async function (this: SyscityWebSocketTransport,agentId: string): Promise<{
    agent_id: string;
    busy: boolean;
    status: string;
    config: Record<string, unknown> | null;
    personality: Record<string, unknown> | null;
  } | null> {
    const res = await this.sendRequestAndWait("agents.get", { agent_id: agentId }) as {
      agent_id: string;
      busy: boolean;
      status: string;
      config: Record<string, unknown> | null;
      personality: Record<string, unknown> | null;
    } | undefined;
    return res || null;
  };

  /** Delete an agent for good: unload it, drop its config overrides, and
   *  remove `agents/<id>/` (personality, workspace, data, memory). */
  proto.purgeAgent = async function (this: SyscityWebSocketTransport, agentId: string): Promise<boolean> {
    const res = await this.sendRequestAndWait("agents.purge", { id: agentId }) as { status?: string } | undefined;
    return res?.status === "purged";
  };

  /** Set an agent's display name and/or emoji (written to IDENTITY.md /
   *  SOUL.md). Returns the values the registry now serves back. */
  proto.renameAgent = async function (
    this: SyscityWebSocketTransport,
    agentId: string,
    fields: { displayName?: string; emoji?: string },
  ): Promise<{ display_name: string; emoji: string } | null> {
    const res = await this.sendRequestAndWait("agents.rename", {
      agent_id: agentId,
      display_name: fields.displayName,
      emoji: fields.emoji,
    }) as { display_name: string; emoji: string } | undefined;
    return res || null;
  };
}
