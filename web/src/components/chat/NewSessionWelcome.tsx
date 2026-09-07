/** New Session welcome page content (the message area shown when the sidebar
 *  New Session toggle is armed). Presentational only — the composer below it
 *  is the chat area's own component, provided by ChatContent, so the first
 *  message sent here runs through the normal runtime path (which creates the
 *  real session lazily; see transport.armNewSession / run()).
 *
 *  When an agent was summoned from the sidebar (no session yet), its identity
 *  is shown instead of the generic greeting; the session is still only
 *  created on the first message. */
import { useChatStore } from "@/stores/chatStore";

export function NewSessionWelcome() {
  const pendingAgent = useChatStore((s) => s.pendingAgent);

  if (pendingAgent) {
    return (
      <div className="text-center">
        <span className="text-5xl block mb-4" aria-hidden="true">
          {pendingAgent.emoji}
        </span>
        <p className="text-primary text-base font-medium">
          {pendingAgent.display_name} is ready
        </p>
        <p className="text-secondary text-sm mt-1.5">
          Send a message to start a session with {pendingAgent.display_name}
        </p>
      </div>
    );
  }

  return (
    <div className="text-center">
      <img
        src="/syscity.png"
        alt="Syscity"
        className="w-16 h-16 mx-auto mb-4"
        draggable={false}
      />
      <p className="text-primary text-base font-medium">
        How can I help today?
      </p>
      <p className="text-secondary text-sm mt-1.5">
        Type your message or press / for commands
      </p>
    </div>
  );
}
