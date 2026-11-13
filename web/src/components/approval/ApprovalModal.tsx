import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { Modal } from "@/components/ui/Modal";
import { Input } from "@/components/ui/Input";
import { Button } from "@/components/ui/Button";

/** A tool call that the human must approve before the agent continues. */
export interface ApprovalPrompt {
  approval_id: string;
  tool_name: string;
  requested_by: string;
  /** "Low" | "Medium" | "High" | "Critical" */
  risk_level: string;
  message: string;
}

interface ApprovalModalProps {
  prompt: ApprovalPrompt;
  /**
   * Approve or deny; the agent's turn resumes with the decision.
   *
   * `remember` records the call as a `[permissions].allow` rule, so the same
   * command is not asked about again ("always allow").
   */
  onDecide: (
    decision: "approve" | "deny",
    reason?: string,
    remember?: boolean
  ) => void;
  /** Dismiss without deciding (the approval stays pending server-side). */
  onDismiss: () => void;
}

const RISK_BADGE: Record<string, string> = {
  Low: "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-300",
  Medium: "bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-300",
  High: "bg-orange-100 text-orange-700 dark:bg-orange-900/30 dark:text-orange-300",
  Critical: "bg-red-100 text-red-700 dark:bg-red-900/30 dark:text-red-300",
};

export function ApprovalModal({ prompt, onDecide, onDismiss }: ApprovalModalProps) {
  const { t } = useTranslation("approval");
  const [reason, setReason] = useState("");
  const [showReason, setShowReason] = useState(false);
  const [busy, setBusy] = useState<"approve" | "approve_always" | "deny" | null>(null);

  const badge = RISK_BADGE[prompt.risk_level] ?? RISK_BADGE.Medium;

  // Escape defers the question, exactly like the close button: the approval
  // stays pending on the gateway and the badge keeps counting it. A modal a
  // human cannot get out of invites answering to make it go away.
  //
  // Captured at the document, so Escape does not also reach the composer or
  // the slash palette behind the dialog.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || busy !== null) return;
      // While writing a deny reason, Escape backs out of that field first —
      // dismissing the whole dialog would throw the text away.
      if (showReason && (e.target as HTMLElement | null)?.tagName === "INPUT") {
        setShowReason(false);
        setReason("");
      } else {
        onDismiss();
      }
      e.preventDefault();
      e.stopPropagation();
    };
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  }, [onDismiss, busy, showReason]);

  const decide = (decision: "approve" | "deny", remember = false) => {
    setBusy(decision === "approve" && remember ? "approve_always" : decision);
    onDecide(decision, reason.trim() || undefined, remember || undefined);
  };

  return (
    <Modal>
      <div className="mb-4" data-testid="approval-dialog">
        <div className="flex items-start gap-2 mb-2">
          <div className="flex items-center gap-2 min-w-0">
            <h3 className="text-sm font-semibold">{t("ApprovalModal.title")}</h3>
            <span
              className={`text-[10px] font-medium px-1.5 py-0.5 rounded ${badge}`}
              data-testid="approval-risk"
            >
              {prompt.risk_level}
            </span>
          </div>
          {/* Defers the request rather than answering it — same as Escape. */}
          <button
            onClick={onDismiss}
            disabled={busy !== null}
            aria-label={t("ApprovalModal.dismiss")}
            title={t("ApprovalModal.dismissTitle")}
            data-testid="approval-close"
            className="ml-auto -mt-1 -mr-1 p-1 rounded-md text-secondary hover:text-primary hover:bg-black/5 dark:hover:bg-white/10 transition disabled:opacity-50"
          >
            <X size={14} />
          </button>
        </div>
        <p className="text-xs text-secondary mb-1">
          <span className="font-medium text-primary">{prompt.tool_name}</span>
          {t("ApprovalModal.by")}
          <span className="text-primary">{prompt.requested_by}</span>
        </p>
        <p className="text-sm text-primary whitespace-pre-wrap mt-2">
          {prompt.message}
        </p>
      </div>

      {showReason && (
        <div className="mb-4">
          <Input
            value={reason}
            onChange={(e) => setReason(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !busy) decide("deny");
            }}
            placeholder={t("ApprovalModal.denyReasonPlaceholder")}
            autoFocus
          />
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <Button
          variant="primary-md"
          disabled={busy !== null}
          onClick={() => decide("approve")}
          data-testid="approval-approve"
        >
          {busy === "approve" ? t("ApprovalModal.approving") : t("ApprovalModal.approve")}
        </Button>
        {/* Same decision, plus a persisted `[permissions].allow` rule for this
            exact call — the "don't ask me about this again" escape hatch. */}
        <Button
          variant="ghost"
          disabled={busy !== null}
          onClick={() => decide("approve", true)}
          title={t("ApprovalModal.alwaysTitle")}
          data-testid="approval-always"
        >
          {busy === "approve_always"
            ? t("ApprovalModal.approving")
            : t("ApprovalModal.always")}
        </Button>
        {showReason ? (
          <Button
            variant="ghost"
            disabled={busy !== null}
            onClick={() => decide("deny")}
            className="text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-900/20"
            data-testid="approval-deny"
          >
            {busy === "deny" ? t("ApprovalModal.denying") : t("ApprovalModal.deny")}
          </Button>
        ) : (
          <Button
            variant="ghost"
            onClick={() => setShowReason(true)}
            data-testid="approval-deny-dots"
          >
            {t("ApprovalModal.denyDots")}
          </Button>
        )}
      </div>

      {/* Dismissing is not a decision. Saying so is the difference between a
          close button that looks like it answers and one that is understood to
          leave the agent waiting. */}
      <p className="mt-3 text-[11px] text-secondary/80">
        {t("ApprovalModal.dismissHelp")}
      </p>
    </Modal>
  );
}
