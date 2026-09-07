import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { AddModelForm } from "@/components/settings/AddModelForm";
import { cloudLogin, cloudStatus } from "@/lib/cloud";

interface WelcomeScreenProps {
  transport: SyscityWebSocketTransport;
  onComplete: () => void;
}

export function WelcomeScreen({ transport, onComplete }: WelcomeScreenProps) {
  const { t } = useTranslation("onboarding");
  // Only surface the cloud sign-in when the backend was built with the cloud
  // feature AND has it enabled. In a default build the endpoint 404s and this
  // stays hidden — no broken button.
  const [cloudAvailable, setCloudAvailable] = useState(false);

  useEffect(() => {
    cloudStatus()
      .then((s) => setCloudAvailable(Boolean(s.enabled)))
      .catch(() => setCloudAvailable(false));
  }, []);

  return (
    <div
      className="flex overflow-y-auto bg-page text-primary"
      style={{
        paddingTop: "env(safe-area-inset-top)",
        paddingBottom: "env(safe-area-inset-bottom)",
        minHeight: "100lvh",
      }}
    >
      {/* m-auto centers when there is room; with overflow the margins collapse
          to 0 so the form stays reachable (and scrollable) on small phones. */}
      <div className="m-auto w-full max-w-2xl px-6 py-8">
        <div className="flex flex-col items-center mb-8">
          <img src="/syscity.png" alt="Syscity" className="w-24 h-24 object-contain mb-6" />
          <h1 className="text-3xl font-semibold mb-2">{t("WelcomeScreen.title")}</h1>
          <p className="text-secondary text-sm">
            {t("WelcomeScreen.configureModel")}
            {cloudAvailable ? t("WelcomeScreen.cloudSuffix") : "."}
          </p>
        </div>
        <div className="rounded-lg bg-card border border-subtle p-2">
          <AddModelForm transport={transport} onAdded={onComplete} />
        </div>
        {cloudAvailable && (
          <>
            <div className="my-6 flex items-center gap-3 text-xs text-tertiary">
              <div className="h-px flex-1 bg-subtle" />
              {t("WelcomeScreen.or")}
              <div className="h-px flex-1 bg-subtle" />
            </div>
            <button
              type="button"
              onClick={() => cloudLogin("github")}
              className="w-full rounded-lg border border-subtle bg-card px-4 py-3 text-sm font-semibold text-primary transition hover:border-primary-500 hover:text-primary-500"
            >
              {t("WelcomeScreen.cloudSignIn")}
            </button>
          </>
        )}
      </div>
    </div>
  );
}
