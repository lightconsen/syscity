/** New Session welcome page content (the message area shown when the sidebar
 *  New Session toggle is armed). Presentational only — the composer below it
 *  is the chat area's own component, provided by ChatContent, so the first
 *  message sent here runs through the normal runtime path (which creates the
 *  real session lazily; see transport.armNewSession / run()).
 *
 *  When an agent was summoned from the sidebar (no session yet), its identity
 *  is shown instead of the generic greeting; the session is still only
 *  created on the first message. */
import { useTranslation } from "react-i18next";
import { useChatStore } from "@/stores/chatStore";

export function NewSessionWelcome() {
  const pendingAgent = useChatStore((s) => s.pendingAgent);
  const { t } = useTranslation("chat");

  if (pendingAgent) {
    return (
      <div className="text-center">
        <span className="text-5xl block mb-4" aria-hidden="true">
          {pendingAgent.emoji}
        </span>
        <p className="text-primary text-base font-medium">
          {t("NewSessionWelcome.agentReady", { name: pendingAgent.display_name })}
        </p>
        <p className="text-secondary text-sm mt-1.5">
          {t("NewSessionWelcome.sendToStart", { name: pendingAgent.display_name })}
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
        {t("NewSessionWelcome.greeting")}
      </p>
      <p className="text-secondary text-sm mt-1.5">
        {t("NewSessionWelcome.hint")}
      </p>
    </div>
  );
}
