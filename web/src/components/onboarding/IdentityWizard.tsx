import { useState } from "react";
import { useTranslation } from "react-i18next";
import type {
  OnboardingPayload,
  SyscityWebSocketTransport,
} from "@/SyscityWebSocketTransport";
import { Input, INPUT_CLASS } from "@/components/ui/Input";

const EMOJI_SUGGESTIONS = [
  "🦑", "🐙", "🦊", "🐺", "🐱", "🐼", "🦁", "🐳",
];

interface IdentityWizardProps {
  transport: SyscityWebSocketTransport;
  onComplete: () => void;
}

/**
 * First-launch identity form.
 *
 * Collects the agent's name / vibe / emoji and the user's preferred name /
 * city / context, then submits everything to the gateway via the WS
 * `onboarding.apply` method. All fields are optional — an empty field is
 * omitted and the backend writes a sensible default.
 */
export function IdentityWizard({ transport, onComplete }: IdentityWizardProps) {
  const { t } = useTranslation("onboarding");
  const [name, setName] = useState("");
  const [vibe, setVibe] = useState("");
  const [emoji, setEmoji] = useState("🦑");
  const [userName, setUserName] = useState("");
  const [city, setCity] = useState("");
  const [userContext, setUserContext] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState("");

  const handleSubmit = async () => {
    setError("");
    const payload: OnboardingPayload = {
      name: name.trim() || undefined,
      vibe: vibe.trim() || undefined,
      emoji: emoji.trim() || undefined,
      user_name: userName.trim() || undefined,
      city: city.trim() || undefined,
      user_context: userContext.trim() || undefined,
    };
    setSubmitting(true);
    const res = await transport.applyOnboarding(payload);
    setSubmitting(false);
    if (res.ok) {
      onComplete();
    } else {
      setError(res.error || t("IdentityWizard.errSave"));
    }
  };

  return (
    <div
      className="flex overflow-y-auto bg-page text-primary"
      style={{
        paddingTop: "env(safe-area-inset-top)",
        paddingBottom: "env(safe-area-inset-bottom)",
        minHeight: "100lvh",
      }}
    >
      <div className="m-auto w-full max-w-2xl px-6 py-8">
        <div className="flex flex-col items-center mb-8">
          <img src="/syscity.png" alt="Syscity" className="w-24 h-24 object-contain mb-6" />
          <h1 className="text-3xl font-semibold mb-2">{t("IdentityWizard.title")}</h1>
          <p className="text-secondary text-sm">
            {t("IdentityWizard.subtitle")}
          </p>
        </div>

        <div className="rounded-lg bg-card border border-subtle p-4 space-y-4">
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
            <Input
              label={t("IdentityWizard.agentName")}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Syscity"
            />
            <Input
              label={t("IdentityWizard.vibe")}
              value={vibe}
              onChange={(e) => setVibe(e.target.value)}
              placeholder="curious, direct, playful"
            />
          </div>

          <div>
            <label className="block text-xs text-secondary mb-1">{t("IdentityWizard.emoji")}</label>
            <div className="flex items-center gap-2">
              <Input
                value={emoji}
                onChange={(e) => setEmoji(e.target.value)}
                placeholder="🦑"
                className="w-20 text-center"
              />
              <div className="flex flex-wrap gap-1">
                {EMOJI_SUGGESTIONS.map((e) => (
                  <button
                    key={e}
                    type="button"
                    onClick={() => setEmoji(e)}
                    className={`w-8 h-8 rounded-lg border text-lg leading-none transition ${
                      emoji === e
                        ? "border-primary-400 bg-primary-100 dark:bg-primary-900/30"
                        : "border-subtle hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
                    }`}
                    aria-label={t("IdentityWizard.useEmoji", { emoji: e })}
                  >
                    {e}
                  </button>
                ))}
              </div>
            </div>
          </div>

          <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
            <Input
              label={t("IdentityWizard.yourName")}
              value={userName}
              onChange={(e) => setUserName(e.target.value)}
              placeholder="Alice"
            />
            <Input
              label={t("IdentityWizard.city")}
              value={city}
              onChange={(e) => setCity(e.target.value)}
              placeholder="Shanghai"
            />
          </div>

          <div>
            <label className="block text-xs text-secondary mb-1">{t("IdentityWizard.aboutYou")}</label>
            <textarea
              className={`${INPUT_CLASS} min-h-20 resize-y`}
              value={userContext}
              onChange={(e) => setUserContext(e.target.value)}
              placeholder={t("IdentityWizard.aboutYouPlaceholder")}
            />
          </div>

          {error && (
            <div className="text-xs text-red-600 dark:text-red-400">{error}</div>
          )}

          <div className="flex justify-end gap-2">
            <button
              type="button"
              onClick={handleSubmit}
              disabled={submitting}
              className="px-4 py-1.5 rounded-md bg-primary-500 hover:bg-primary-600 disabled:opacity-50 text-white text-xs font-medium transition"
            >
              {submitting ? t("IdentityWizard.saving") : t("IdentityWizard.saveContinue")}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
