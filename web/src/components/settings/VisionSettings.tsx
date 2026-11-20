import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ModelInfo } from "@/transportTypes";
import type { SyscityConfig } from "@/components/settings/useSettingsData";
import { Section } from "@/components/ui/Section";

const INPUT_CLASS =
  "w-full text-sm border border-subtle rounded-lg px-3 py-2 bg-card text-primary";
const SELECT_CLASS = INPUT_CLASS;

const MODES = ["auto", "local", "api", "off"] as const;

interface VisionSettingsProps {
  config: SyscityConfig;
  models: ModelInfo[];
  update: (path: string, value: unknown) => Promise<void>;
  /** Re-read the config after a write, so `ocr_installed` stops describing
   *  the preset the page loaded with. */
  onRefresh: () => Promise<void>;
}

export function VisionSettings({ config, models, update, onRefresh }: VisionSettingsProps) {
  const { t } = useTranslation("settings");
  const vision = config.computer?.vision;

  // The model directory is a text field, but a write per keystroke would
  // reinstall the shared settings handle on every character and rewrite the
  // config file each time. Commit on blur / Enter instead.
  const [modelDir, setModelDir] = useState(vision?.model_dir ?? "");
  useEffect(() => {
    setModelDir(vision?.model_dir ?? "");
  }, [vision?.model_dir]);
  const commitModelDir = () => {
    if ((vision?.model_dir ?? "") !== modelDir) void update("computer.vision.model_dir", modelDir);
  };

  // Same for the timeout: every keystroke would write the config file, and
  // clearing the box to retype it would send 0, which the backend refuses.
  const [timeout, setTimeout] = useState(String(vision?.timeout_secs ?? ""));
  useEffect(() => {
    setTimeout(String(vision?.timeout_secs ?? ""));
  }, [vision?.timeout_secs]);
  const commitTimeout = () => {
    const secs = Math.round(Number(timeout));
    if (!Number.isFinite(secs) || secs <= 0) {
      setTimeout(String(vision?.timeout_secs ?? ""));
      return;
    }
    if (secs !== vision?.timeout_secs) void update("computer.vision.timeout_secs", secs);
  };

  const mode = vision?.mode ?? "auto";
  // A hand-edited config can hold a typo, and the resolver treats an unknown
  // string as "auto". Show that, rather than a hint key that renders raw.
  const shownMode = (MODES as readonly string[]).includes(mode) ? mode : "auto";
  const provider = vision?.provider ?? "";
  const model = vision?.model ?? "";

  // Only models from the chosen provider are offered: the API rung resolves
  // `provider` and `model` as a pair, and a model id from another provider
  // would be sent to the wrong backend.
  const providerNames = Array.from(new Set(models.map((m) => m.provider))).sort();
  const providerModels = models.filter((m) => m.provider === provider);
  // The id is what gets stored; the name is what a person recognises.
  const providerLabel = (id: string) =>
    models.find((m) => m.provider === id)?.provider_name || id;

  // Keep what a switch would otherwise discard: choosing "api" from "auto"
  // must not blank a configured pair, and clearing the provider when a model
  // is still set would leave a half-configured rung.
  const changeMode = async (next: string) => {
    if (next === "api" && !provider && providerNames.length > 0) {
      await update("computer.vision.provider", providerNames[0]);
    }
    await update("computer.vision.mode", next);
  };

  const changeProvider = async (next: string) => {
    await update("computer.vision.provider", next);
    // The previous model does not belong to the new provider.
    if (model && !models.some((m) => m.provider === next && m.id === model)) {
      await update("computer.vision.model", "");
    }
  };

  return (
    <div className="space-y-5">
      <Section title={t("VisionSettings.mode")}>
        <select
          value={shownMode}
          onChange={(e) => void changeMode(e.target.value)}
          className={SELECT_CLASS}
        >
          {MODES.map((m) => (
            <option key={m} value={m}>
              {t(`VisionSettings.mode_${m}`)}
            </option>
          ))}
        </select>
        <p className="mt-2 text-xs text-secondary">{t(`VisionSettings.modeHint_${shownMode}`)}</p>
      </Section>

      {/* The API rung is only consulted in "api", or in "auto" with no local
          model. Hidden in "local" and "off" rather than disabled, because a
          disabled field still reads as "this matters here". */}
      {(shownMode === "api" || shownMode === "auto") && (
        <Section title={t("VisionSettings.apiRung")}>
          {models.length === 0 ? (
            <p className="text-sm text-secondary">{t("VisionSettings.noModels")}</p>
          ) : (
            <div className="space-y-3">
              <div className="flex items-center gap-3">
                <label className="w-28 text-sm text-secondary shrink-0">
                  {t("VisionSettings.provider")}
                </label>
                <select
                  value={provider}
                  onChange={(e) => void changeProvider(e.target.value)}
                  className={SELECT_CLASS}
                >
                  <option value="">{t("VisionSettings.none")}</option>
                  {providerNames.map((p) => (
                    <option key={p} value={p}>
                      {providerLabel(p)}
                    </option>
                  ))}
                </select>
              </div>
              <div className="flex items-center gap-3">
                <label className="w-28 text-sm text-secondary shrink-0">
                  {t("VisionSettings.model")}
                </label>
                <select
                  value={model}
                  onChange={(e) => void update("computer.vision.model", e.target.value)}
                  disabled={!provider}
                  className={SELECT_CLASS}
                >
                  <option value="">{t("VisionSettings.none")}</option>
                  {providerModels.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.name}
                    </option>
                  ))}
                </select>
              </div>
              <p className="text-xs text-secondary">{t("VisionSettings.apiHint")}</p>
            </div>
          )}
        </Section>
      )}

      {/* Local recognition. `ocr_installed` is about the *configured* preset,
          not "OCR is installed" — a machine holding only the english files
          cannot read a Chinese screen. */}
      <Section title={t("VisionSettings.ocrPreset")}>
        <select
          value={vision?.ocr ?? "ppocrv3-en"}
          onChange={(e) => {
            void update("computer.vision.ocr", e.target.value).then(onRefresh);
          }}
          className={SELECT_CLASS}
        >
          {(vision?.ocr_presets ?? []).map((p) => (
            <option key={p.key} value={p.key}>
              {p.label}
            </option>
          ))}
        </select>
        <p className="mt-2 text-xs text-secondary">
          {vision?.ocr_installed
            ? t("VisionSettings.ocrInstalled")
            : t("VisionSettings.ocrMissing")}
        </p>
      </Section>

      <Section title={t("VisionSettings.modelDir")}>
        <input
          type="text"
          value={modelDir}
          onChange={(e) => setModelDir(e.target.value)}
          onBlur={commitModelDir}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.currentTarget.blur();
            }
          }}
          placeholder={t("VisionSettings.modelDirPlaceholder")}
          className={INPUT_CLASS}
        />
        <p className="mt-2 text-xs text-secondary">{t("VisionSettings.modelDirHint")}</p>
      </Section>

      <Section title={t("VisionSettings.timeout")}>
        <input
          type="number"
          min={1}
          value={timeout}
          onChange={(e) => setTimeout(e.target.value)}
          onBlur={commitTimeout}
          onKeyDown={(e) => {
            if (e.key === "Enter") e.currentTarget.blur();
          }}
          className={INPUT_CLASS}
        />
        <p className="mt-2 text-xs text-secondary">{t("VisionSettings.timeoutHint")}</p>
      </Section>
    </div>
  );
}
