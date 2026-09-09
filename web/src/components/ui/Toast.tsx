import { create } from "zustand";
import { CheckCircle2, XCircle } from "lucide-react";

/** App-global toast. Mounted once in App (Toaster); any component can push
 *  via useToastStore.getState().push(...) — e.g. marketplace install actions
 *  with an embedded follow-up button ("去试试"). */

export interface ToastAction {
  label: string;
  onClick: () => void;
}

export interface ToastItem {
  id: number;
  kind: "success" | "error";
  message: string;
  action?: ToastAction;
}

interface ToastState {
  toasts: ToastItem[];
  push: (kind: ToastItem["kind"], message: string, action?: ToastAction) => void;
  dismiss: (id: number) => void;
}

const TOAST_TTL_MS = 4000;

export const useToastStore = create<ToastState>((set) => ({
  toasts: [],
  push: (kind, message, action) => {
    const id = Date.now() + Math.random();
    set((s) => ({ toasts: [...s.toasts.slice(-2), { id, kind, message, action }] }));
    setTimeout(() => {
      set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
    }, TOAST_TTL_MS);
  },
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}));

/** Imperative helper so non-hook callers don't need the hook. */
export function pushToast(
  kind: ToastItem["kind"],
  message: string,
  action?: ToastAction,
): void {
  useToastStore.getState().push(kind, message, action);
}

export function Toaster() {
  const toasts = useToastStore((s) => s.toasts);
  const dismiss = useToastStore((s) => s.dismiss);
  if (toasts.length === 0) return null;

  return (
    <div className="fixed bottom-6 right-6 z-[60] flex flex-col gap-2">
      {toasts.map((t) => (
        <div
          key={t.id}
          role="status"
          className="flex items-center gap-2.5 rounded-lg bg-card border border-subtle shadow-lg px-3.5 py-2.5 max-w-sm"
        >
          {t.kind === "success" ? (
            <CheckCircle2 size={15} className="text-emerald-500 shrink-0" />
          ) : (
            <XCircle size={15} className="text-red-500 shrink-0" />
          )}
          <span className="text-sm text-primary">{t.message}</span>
          {t.action && (
            <button
              onClick={() => {
                t.action?.onClick();
                dismiss(t.id);
              }}
              className="text-xs font-medium text-primary-500 hover:underline shrink-0"
            >
              {t.action.label}
            </button>
          )}
          <button
            aria-label="Dismiss"
            onClick={() => dismiss(t.id)}
            className="text-secondary/60 hover:text-secondary text-xs ml-1 shrink-0"
          >
            ✕
          </button>
        </div>
      ))}
    </div>
  );
}
