import type { SyscityWebSocketTransport } from "../transportCore";

// Domain mixin: sessions RPC method implementations (installed on the prototype by
// the facade `SyscityWebSocketTransport.ts`). Signatures are merged onto the
// class type in `transportCore.ts`.
export function install(proto: typeof SyscityWebSocketTransport.prototype): void {
  proto.setSessionModel = async function (this: SyscityWebSocketTransport,
    sessionId: string,
    model: string | null
  ): Promise<boolean> {
    try {
      await this.sendRequestAndWait("sessions.set_model", {
        session_id: sessionId,
        model,
      });
      return true;
    } catch {
      return false;
    }
  };
  proto.setSessionPinned = async function (this: SyscityWebSocketTransport,
    sessionId: string,
    pinned: boolean
  ): Promise<boolean> {
    try {
      await this.sendRequestAndWait("sessions.set_pinned", {
        session_id: sessionId,
        pinned,
      });
      return true;
    } catch {
      return false;
    }
  };
  /**
   * `sessions.set_mode` — this session's permission mode override.
   *
   * The override is ephemeral: it lives on the gateway until the process
   * restarts, then falls back to the configured `[permissions].mode`. The
   * gateway is the authority — `bypass` without `[permissions].allow_bypass`
   * comes back as `BYPASS_DISABLED`, which this reports as `false` so the
   * caller can revert its optimistic UI.
   */
  proto.setSessionMode = async function (this: SyscityWebSocketTransport,
    sessionId: string,
    mode: "default" | "accept_edits" | "plan" | "bypass"
  ): Promise<boolean> {
    try {
      await this.sendRequestAndWait("sessions.set_mode", {
        session_id: sessionId,
        mode,
      });
      return true;
    } catch {
      return false;
    }
  };
  /**
   * `sessions.get_mode` — the mode the session really runs in.
   *
   * Unlike reading `permission_mode` off `sessions.list`, this answers for a
   * conversation the store has never seen: the composer's session has an
   * override before its first message creates a row.
   */
  proto.getSessionMode = async function (this: SyscityWebSocketTransport,
    sessionId: string
  ): Promise<{ mode: "default" | "accept_edits" | "plan" | "bypass"; allowBypass: boolean } | null> {
    try {
      const res = (await this.sendRequestAndWait("sessions.get_mode", {
        session_id: sessionId,
      })) as { mode?: string; allow_bypass?: boolean } | undefined;
      const mode = res?.mode;
      if (!mode) return null;
      return {
        mode: mode as "default" | "accept_edits" | "plan" | "bypass",
        allowBypass: res?.allow_bypass === true,
      };
    } catch {
      return null;
    }
  };
  /**
   * `sessions.status` — is this conversation's turn still running?
   *
   * The gateway answers from its own record of turns in flight, which is the
   * only thing a client that did not start the turn can rely on: after a
   * reload, a reconnect, or a turn started from the CLI or a channel, nothing
   * local says the conversation is busy — and the view then looks stopped.
   * `awaiting_approval` separates "working" from "parked on a human", so the
   * UI can point at the approval instead of spinning.
   */
  proto.getSessionStatus = async function (
    this: SyscityWebSocketTransport,
    sessionId: string
  ): Promise<{ running: boolean; awaitingApproval: boolean } | null> {
    try {
      const res = (await this.sendRequestAndWait("sessions.status", {
        session_id: sessionId,
      })) as { running?: boolean; awaiting_approval?: boolean } | undefined;
      return {
        running: res?.running === true,
        awaitingApproval: res?.awaiting_approval === true,
      };
    } catch {
      return null;
    }
  };
  proto.renameSession = async function (this: SyscityWebSocketTransport,sessionId: string, name: string): Promise<boolean> {
    try {
      await this.sendRequestAndWait("sessions.rename", { session_id: sessionId, name });
      return true;
    } catch {
      return false;
    }
  };
  proto.respondToAsk = async function (this: SyscityWebSocketTransport,askId: string, response: string): Promise<boolean> {
    try {
      await this.sendRequestAndWait("ask.respond", { ask_id: askId, response });
      return true;
    } catch {
      return false;
    }
  };
  proto.vote = async function (this: SyscityWebSocketTransport,
    turnId: string,
    vote: "up" | "down",
    opts?: { input?: string; response?: string; comment?: string }
  ): Promise<boolean> {
    try {
      await this.sendRequestAndWait("feedback.vote", {
        turn_id: turnId,
        vote,
        ...(opts?.comment ? { comment: opts.comment } : {}),
        ...(opts?.input ? { input: opts.input } : {}),
        ...(opts?.response ? { response: opts.response } : {}),
      });
      return true;
    } catch {
      return false;
    }
  };
}
