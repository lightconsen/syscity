import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

interface AgentRenameDialogProps {
  /** Current display name, used to prefill the field. */
  displayName: string;
  /** Current emoji, used to prefill the field. */
  emoji: string;
  /** Disable the inputs + save button while the write is in flight. */
  busy?: boolean;
  onSave: (fields: { displayName: string; emoji: string }) => void;
  onCancel: () => void;
}

/**
 * Rename dialog for an agent: the display name (written to IDENTITY.md) and
 * its emoji (written to SOUL.md's frontmatter).
 */
export function AgentRenameDialog({
  displayName,
  emoji,
  busy = false,
  onSave,
  onCancel,
}: AgentRenameDialogProps) {
  const { t } = useTranslation("chat");
  const [name, setName] = useState(displayName);
  const [icon, setIcon] = useState(emoji);
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    nameRef.current?.focus();
    nameRef.current?.select();
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel]);

  const trimmed = name.trim();
  // Both fields are required: the backend leaves the emoji alone when it is
  // sent empty, so saving one would silently not clear the other.
  const canSave =
    trimmed.length > 0 &&
    icon.trim().length > 0 &&
    (trimmed !== displayName || icon.trim() !== emoji);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
      role="dialog"
      aria-modal="true"
      aria-label={t("AgentRenameDialog.title")}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <form
        className="bg-card rounded-xl p-5 max-w-sm w-full mx-4 shadow-xl"
        onSubmit={(e) => {
          e.preventDefault();
          if (canSave && !busy) onSave({ displayName: trimmed, emoji: icon.trim() });
        }}
      >
        <h3 className="text-sm font-semibold text-primary">
          {t("AgentRenameDialog.title")}
        </h3>
        <div className="mt-4 space-y-3">
          <div>
            <label className="block text-xs text-secondary mb-1" htmlFor="agent-rename-name">
              {t("AgentRenameDialog.nameLabel")}
            </label>
            <input
              id="agent-rename-name"
              ref={nameRef}
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              disabled={busy}
              className="w-full text-sm px-2 py-1.5 rounded-md bg-card text-primary border border-subtle focus:outline-none focus:ring-2 focus:ring-primary-500/20 disabled:opacity-50"
            />
          </div>
          <div>
            <label className="block text-xs text-secondary mb-1" htmlFor="agent-rename-emoji">
              {t("AgentRenameDialog.emojiLabel")}
            </label>
            <input
              id="agent-rename-emoji"
              type="text"
              value={icon}
              onChange={(e) => setIcon(e.target.value)}
              disabled={busy}
              className="w-20 text-sm px-2 py-1.5 rounded-md bg-card text-primary border border-subtle focus:outline-none focus:ring-2 focus:ring-primary-500/20 disabled:opacity-50"
            />
            <p className="mt-1 text-[11px] text-secondary/70">
              {t("AgentRenameDialog.hint")}
            </p>
          </div>
        </div>
        <div className="mt-4 flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            disabled={busy}
            className="px-3 py-1.5 rounded-md text-xs bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition disabled:opacity-50"
          >
            {t("AgentRenameDialog.cancel")}
          </button>
          <button
            type="submit"
            disabled={!canSave || busy}
            className="px-3 py-1.5 rounded-md text-xs font-medium bg-primary-500 hover:bg-primary-600 text-white transition disabled:opacity-50"
          >
            {t("AgentRenameDialog.save")}
          </button>
        </div>
      </form>
    </div>
  );
}
