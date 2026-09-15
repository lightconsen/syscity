import { useEffect } from "react";
import { useTranslation } from "react-i18next";

interface ConfirmDialogProps {
  title: string;
  /** Body copy — the consequence of confirming, stated plainly. */
  message: string;
  confirmLabel?: string;
  cancelLabel?: string;
  /** Paint the confirm button as destructive (irreversible actions). */
  destructive?: boolean;
  /** Disable both buttons while the action is in flight. */
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

/**
 * Confirmation dialog for destructive actions.
 *
 * Preferred over `window.confirm`: the native prompt is jarring inside the
 * Tauri WebView and cannot carry rich body copy.
 */
export function ConfirmDialog({
  title,
  message,
  confirmLabel,
  cancelLabel,
  destructive = false,
  busy = false,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const { t } = useTranslation("common");

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
      if (e.key === "Enter") onConfirm();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, onConfirm]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <div className="bg-card rounded-xl p-5 max-w-sm w-full mx-4 shadow-xl">
        <h3 className="text-sm font-semibold text-primary">{title}</h3>
        <p className="mt-2 text-xs text-secondary whitespace-pre-line">{message}</p>
        <div className="mt-4 flex justify-end gap-2">
          <button
            onClick={onCancel}
            disabled={busy}
            className="px-3 py-1.5 rounded-md text-xs bg-sidebar hover:bg-black/5 dark:hover:bg-white/5 text-secondary transition disabled:opacity-50"
          >
            {cancelLabel ?? t("ConfirmDialog.cancel")}
          </button>
          <button
            onClick={onConfirm}
            disabled={busy}
            className={`px-3 py-1.5 rounded-md text-xs font-medium text-white transition disabled:opacity-50 ${
              destructive ? "bg-red-600 hover:bg-red-700" : "bg-primary-500 hover:bg-primary-600"
            }`}
          >
            {confirmLabel ?? t("ConfirmDialog.confirm")}
          </button>
        </div>
      </div>
    </div>
  );
}
