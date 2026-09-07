import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/ui/Modal";
import { Input } from "@/components/ui/Input";
import { Button } from "@/components/ui/Button";

/** A question from the agent that the human must answer. */
export interface AskPrompt {
  ask_id: string;
  session_id: string;
  question: string;
  options: string[];
  required: boolean;
  default?: string;
}

interface AskModalProps {
  prompt: AskPrompt;
  /** Submit the answer back to the agent. */
  onRespond: (response: string) => void;
  /** Dismiss without answering (the agent's ask times out server-side). */
  onDismiss: () => void;
}

export function AskModal({ prompt, onRespond, onDismiss }: AskModalProps) {
  const { t } = useTranslation("ask");
  const [selected, setSelected] = useState<string | null>(
    prompt.default ?? (prompt.options.length > 0 ? prompt.options[0] ?? null : null)
  );
  const [freeText, setFreeText] = useState(prompt.default ?? "");

  const hasOptions = prompt.options.length > 0;
  const answer = hasOptions ? selected ?? "" : freeText;
  const canSubmit = !prompt.required || answer.trim().length > 0;

  return (
    <Modal>
      <h3 className="text-sm font-semibold mb-1">{t("AskModal.title")}</h3>
      <p className="text-sm text-primary mb-4 whitespace-pre-wrap">{prompt.question}</p>

      {hasOptions ? (
        <div className="flex flex-col gap-2 mb-4">
          {prompt.options.map((opt) => (
            <button
              key={opt}
              type="button"
              onClick={() => setSelected(opt)}
              className={`text-left px-3 py-2 rounded-lg text-sm border transition-colors ${
                selected === opt
                  ? "border-primary-500 bg-primary-500/10 text-primary"
                  : "border-subtle bg-card text-secondary hover:text-primary"
              }`}
            >
              {opt}
            </button>
          ))}
        </div>
      ) : (
        <div className="mb-4">
          <Input
            value={freeText}
            onChange={(e) => setFreeText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && canSubmit) onRespond(freeText.trim());
            }}
            placeholder={t("AskModal.placeholder")}
            autoFocus
          />
        </div>
      )}

      <div className="flex gap-2">
        <Button
          variant="primary-md"
          disabled={!canSubmit}
          onClick={() => onRespond(answer.trim())}
        >
          {t("AskModal.answer")}
        </Button>
        <Button variant="ghost" onClick={onDismiss}>
          {t("AskModal.notNow")}
        </Button>
      </div>
    </Modal>
  );
}
