import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { ConnectionSettings } from "@/components/settings/ConnectionSettings";

interface ConnectGatewayDialogProps {
  onClose: () => void;
}

/**
 * The gateway-connection form, reachable from the first-launch screens.
 *
 * Settings → Connection already holds every field (local/remote, host, port,
 * TLS, token) and already persists to `~/.syscity/client.toml` — but Settings
 * itself is *behind* the launch gates, and those gates need a working
 * connection to open. A brand-new desktop install therefore had no way to
 * point itself at a remote gateway: the welcome screen wanted a model, the
 * identity wizard wanted an identity, and neither could be fetched from a
 * gateway that was never selected. The only exit was to edit `client.toml` by
 * hand.
 *
 * So this renders the very same component rather than a copy of its fields:
 * one source of truth for what a gateway connection is, and a fixed field
 * here automatically appears on the first-launch path too.
 */
export function ConnectGatewayDialog({ onClose }: ConnectGatewayDialogProps) {
  const { t } = useTranslation("onboarding");

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={t("ConnectGatewayDialog.title")}
      data-testid="connect-gateway-dialog"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="max-h-[85vh] w-full max-w-lg overflow-y-auto rounded-xl bg-card p-5 shadow-xl">
        <div className="mb-4 flex items-start justify-between gap-3">
          <div>
            <h3 className="text-sm font-semibold text-primary">
              {t("ConnectGatewayDialog.title")}
            </h3>
            <p className="mt-1 text-xs text-secondary">
              {t("ConnectGatewayDialog.subtitle")}
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label={t("ConnectGatewayDialog.close")}
            className="shrink-0 rounded-md px-2 py-1 text-xs text-secondary transition hover:bg-black/5 dark:hover:bg-white/5"
          >
            ✕
          </button>
        </div>
        <ConnectionSettings />
      </div>
    </div>
  );
}
