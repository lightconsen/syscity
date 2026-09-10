// Facade: the full transport class (core + domain mixins) + the
// active-transport singleton. All consumers import from here.

import { SyscityWebSocketTransport } from "./transportCore";
import { install as installAgents } from "./transportMixins/agents";
import { install as installApprovals } from "./transportMixins/approvals";
import { install as installChannels } from "./transportMixins/channels";
import { install as installCloud } from "./transportMixins/cloud";
import { install as installConfig } from "./transportMixins/config";
import { install as installConnectors } from "./transportMixins/connectors";
import { install as installCron } from "./transportMixins/cron";
import { install as installDevice } from "./transportMixins/device";
import { install as installEval } from "./transportMixins/eval";
import { install as installKb } from "./transportMixins/kb";
import { install as installMarketplace } from "./transportMixins/marketplace";
import { install as installMcp } from "./transportMixins/mcp";
import { install as installModels } from "./transportMixins/models";
import { install as installSessions } from "./transportMixins/sessions";
import { install as installShortcuts } from "./transportMixins/shortcuts";
import { install as installSkills } from "./transportMixins/skills";
import { install as installUpdate } from "./transportMixins/update";
import { install as installWorkspace } from "./transportMixins/workspace";

installAgents(SyscityWebSocketTransport.prototype);
installApprovals(SyscityWebSocketTransport.prototype);
installChannels(SyscityWebSocketTransport.prototype);
installCloud(SyscityWebSocketTransport.prototype);
installConfig(SyscityWebSocketTransport.prototype);
installConnectors(SyscityWebSocketTransport.prototype);
installCron(SyscityWebSocketTransport.prototype);
installDevice(SyscityWebSocketTransport.prototype);
installEval(SyscityWebSocketTransport.prototype);
installKb(SyscityWebSocketTransport.prototype);
installMarketplace(SyscityWebSocketTransport.prototype);
installMcp(SyscityWebSocketTransport.prototype);
installModels(SyscityWebSocketTransport.prototype);
installSessions(SyscityWebSocketTransport.prototype);
installShortcuts(SyscityWebSocketTransport.prototype);
installSkills(SyscityWebSocketTransport.prototype);
installUpdate(SyscityWebSocketTransport.prototype);
installWorkspace(SyscityWebSocketTransport.prototype);

// ── Active-transport singleton ──────────────────────────────────────────
//
// The app creates one transport in App.tsx and registers it here so that
// standalone modules (lib/cloud.ts, hooks/useUpdate.ts, settings components)
// can drive admin operations over the WebSocket instead of REST.

let activeTransport: SyscityWebSocketTransport | null = null;

/** Register the app's transport instance. */
export function setActiveTransport(t: SyscityWebSocketTransport): void {
  activeTransport = t;
}

/** The registered transport, or null before App registers it. */
export function getActiveTransport(): SyscityWebSocketTransport | null {
  return activeTransport;
}

export { SyscityWebSocketTransport };
export type * from "./transportTypes";
export * from "./transportTypes";
