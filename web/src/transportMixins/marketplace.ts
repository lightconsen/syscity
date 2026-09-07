import type { SyscityWebSocketTransport } from "../transportCore";

// Domain mixin: marketplace RPC method implementations (installed on the prototype by
// the facade `SyscityWebSocketTransport.ts`). Signatures are merged onto the
// class type in `transportCore.ts`.
export function install(proto: typeof SyscityWebSocketTransport.prototype): void {
  proto.getConnectorsCatalog = async function (
    this: SyscityWebSocketTransport,
    lang?: string,
  ): Promise<unknown> {
    // `lang` (e.g. navigator.language) becomes Accept-Language on a cloud
    // catalog sync; omit for no preference (cloud defaults to English).
    return this.sendRequestAndWait("connectors.catalog", lang ? { lang } : {});
  };
}
