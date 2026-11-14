import type { SyscityWebSocketTransport } from "../transportCore";

// Domain mixin: update RPC method implementations (installed on the prototype by
// the facade `SyscityWebSocketTransport.ts`). Signatures are merged onto the
// class type in `transportCore.ts`.
export function install(proto: typeof SyscityWebSocketTransport.prototype): void {
  proto.getUpdateStatus = async function (
    this: SyscityWebSocketTransport,
    refresh = false,
  ): Promise<unknown> {
    return this.sendRequestAndWait("update.status", refresh ? { refresh: true } : {});
  };
  proto.getUpdateProgress = async function (this: SyscityWebSocketTransport,): Promise<unknown> {
    return this.sendRequestAndWait("update.progress", {});
  };
  proto.triggerUpdate = async function (this: SyscityWebSocketTransport,): Promise<unknown> {
    return this.sendRequestAndWait("update.trigger", {});
  };
}
