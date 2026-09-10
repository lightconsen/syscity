import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { useChatStore } from "@/stores/chatStore";

/** Chip pills attached via the composer "+" picker, shown between the text
 *  input and the toolbar. Empty state renders nothing. */
export function ComposerChips() {
  const { t } = useTranslation("chat");
  const chips = useChatStore((s) => s.pendingChips);
  const remove = useChatStore((s) => s.removeComposerChip);
  if (chips.length === 0) return null;

  return (
    <div className="flex flex-wrap gap-1.5 px-3 pt-2">
      {chips.map((c) => (
        <span
          key={`${c.kind}-${c.id}`}
          className="inline-flex items-center gap-1 rounded-full bg-primary-50 dark:bg-primary-900/20 px-2 py-0.5 text-xs text-primary"
        >
          {c.emoji && <span>{c.emoji}</span>}
          <span className="max-w-40 truncate">{c.label}</span>
          <button
            type="button"
            aria-label={t("EntityPicker.removeChip", { label: c.label })}
            onClick={() => remove(c.kind, c.id)}
            className="rounded-full p-0.5 text-secondary/70 hover:text-primary hover:bg-black/[0.06] dark:hover:bg-white/[0.08] transition"
          >
            <X className="w-3 h-3" />
          </button>
        </span>
      ))}
    </div>
  );
}
