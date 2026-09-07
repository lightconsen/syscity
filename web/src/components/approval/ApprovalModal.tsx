import { useState } from "react";
import { useTranslation } from "react-i18next";
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
  /** Approve or deny; the agent's turn resumes with the decision. */
  onDecide: (decision: "approve" | "deny", reason?: string) => void;
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
  const [busy, setBusy] = useState<"approve" | "deny" | null>(null);

  const badge = RISK_BADGE[prompt.risk_level] ?? RISK_BADGE.Medium;

  const decide = (decision: "approve" | "deny") => {
    setBusy(decision);
    onDecide(decision, reason.trim() || undefined);
  };

  return (
    <Modal>
      <div className="mb-4">
        <div className="flex items-center gap-2 mb-2">
          <h3 className="text-sm font-semibold">{t("ApprovalModal.title")}</h3>
          <span
            className={`text-[10px] font-medium px-1.5 py-0.5 rounded ${badge}`}
          >
            {prompt.risk_level}
          </span>
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

      <div className="flex gap-2">
        <Button
          variant="primary-md"
          disabled={busy !== null}
          onClick={() => decide("approve")}
        >
          {busy === "approve" ? t("ApprovalModal.approving") : t("ApprovalModal.approve")}
        </Button>
        {showReason ? (
          <Button
            variant="ghost"
            disabled={busy !== null}
            onClick={() => decide("deny")}
            className="text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-900/20"
          >
            {busy === "deny" ? t("ApprovalModal.denying") : t("ApprovalModal.deny")}
          </Button>
        ) : (
          <Button variant="ghost" onClick={() => setShowReason(true)}>
            {t("ApprovalModal.denyDots")}
          </Button>
        )}
        <Button variant="ghost" onClick={onDismiss}>
          {t("ApprovalModal.later")}
        </Button>
      </div>
    </Modal>
  );
}
