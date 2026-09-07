import { useTranslation } from "react-i18next";

interface McpAuthModalProps {
  authModal: { serverId: string; authUrl: string } | null;
  onCancel: () => Promise<void>;
}

export function McpAuthModal({ authModal, onCancel }: McpAuthModalProps) {
  const { t } = useTranslation("settings");
  if (!authModal) return null;
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div className="bg-card rounded-xl p-6 max-w-md w-full mx-4 shadow-xl">
        <h3 className="text-sm font-semibold mb-2">{t("McpAuthModal.title")}</h3>
        <p className="text-xs text-secondary mb-4">
          {t("McpAuthModal.body")}
        </p>
        <div className="flex gap-2">
          <button
            onClick={() => window.open(authModal.authUrl, "_blank")}
            className="px-3 py-1.5 text-xs font-medium rounded-lg bg-primary-600 text-white hover:opacity-90 transition-opacity"
          >
            {t("McpAuthModal.authorize")}
          </button>
          <button
            onClick={() => onCancel()}
            className="px-3 py-1.5 text-xs font-medium rounded-lg bg-sidebar text-secondary hover:text-primary transition-colors"
          >
            {t("McpAuthModal.cancel")}
          </button>
        </div>
      </div>
    </div>
  );
}
