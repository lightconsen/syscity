import type { SyscityWebSocketTransport } from "../transportCore";

// Domain mixin: connectors RPC method implementations (installed on the
// prototype by the facade `SyscityWebSocketTransport.ts`). Signatures are
// merged onto the class type in `transportCore.ts`.
export function install(proto: typeof SyscityWebSocketTransport.prototype): void {
  proto.listConnectors = async function (
    this: SyscityWebSocketTransport,
  ): Promise<Array<Record<string, unknown>>> {
    const res = (await this.sendRequestAndWait("connectors.list", {})) as
      | { connectors?: Array<Record<string, unknown>> }
      | undefined;
    return res?.connectors ?? [];
  };
  proto.enableConnector = async function (
    this: SyscityWebSocketTransport,
    id: string,
  ): Promise<boolean> {
    try {
      await this.sendRequestAndWait("connectors.enable", { id });
      return true;
    } catch {
      return false;
    }
  };
}
