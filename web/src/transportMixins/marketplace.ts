import type { SyscityWebSocketTransport } from "../transportCore";

// Domain mixin: marketplace RPC method implementations (installed on the prototype by
// the facade `SyscityWebSocketTransport.ts`). Signatures are merged onto the
// class type in `transportCore.ts`.
export function install(proto: typeof SyscityWebSocketTransport.prototype): void {
  proto.getConnectorsCatalog = async function (
    this: SyscityWebSocketTransport,
    lang?: string,
    refresh?: boolean,
  ): Promise<unknown> {
    // `lang` (e.g. navigator.language) becomes Accept-Language on a cloud
    // catalog sync; omit for no preference (cloud defaults to English).
    // `refresh` forces a server-side re-fetch even when a same-language
    // cache exists (the UI's Refresh button).
    const params: Record<string, unknown> = {};
    if (lang) params.lang = lang;
    if (refresh) params.refresh = true;
    return this.sendRequestAndWait("connectors.catalog", params);
  };
}
