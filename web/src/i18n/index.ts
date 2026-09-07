import i18n from "i18next";
import { initReactI18next } from "react-i18next";

import enCommon from "./locales/en/common.json";
import zhCommon from "./locales/zh/common.json";
import enChat from "./locales/en/chat.json";
import zhChat from "./locales/zh/chat.json";
import enSettings from "./locales/en/settings.json";
import zhSettings from "./locales/zh/settings.json";
import enKb from "./locales/en/kb.json";
import zhKb from "./locales/zh/kb.json";
import enMarketplace from "./locales/en/marketplace.json";
import zhMarketplace from "./locales/zh/marketplace.json";
import enWorkspace from "./locales/en/workspace.json";
import zhWorkspace from "./locales/zh/workspace.json";
import enOnboarding from "./locales/en/onboarding.json";
import zhOnboarding from "./locales/zh/onboarding.json";
import enAsk from "./locales/en/ask.json";
import zhAsk from "./locales/zh/ask.json";
import enApproval from "./locales/en/approval.json";
import zhApproval from "./locales/zh/approval.json";
import enUpdate from "./locales/en/update.json";
import zhUpdate from "./locales/zh/update.json";
import enChrome from "./locales/en/chrome.json";
import zhChrome from "./locales/zh/chrome.json";
import enApp from "./locales/en/app.json";
import zhApp from "./locales/zh/app.json";

/** Storage key for the user's explicit language choice (Settings → General). */
export const LANG_STORAGE_KEY = "syscity.lang";

export type AppLang = "en" | "zh";

/** Persisted choice wins; otherwise follow the system language (zh* → zh). */
function detectLang(): AppLang {
  try {
    const stored = localStorage.getItem(LANG_STORAGE_KEY);
    if (stored === "en" || stored === "zh") return stored;
  } catch {
    /* private mode etc. — fall through to system language */
  }
  return navigator.language.toLowerCase().startsWith("zh") ? "zh" : "en";
}

i18n.use(initReactI18next).init({
  resources: {
    en: {
      common: enCommon,
      chat: enChat,
      settings: enSettings,
      kb: enKb,
      marketplace: enMarketplace,
      workspace: enWorkspace,
      onboarding: enOnboarding,
      ask: enAsk,
      approval: enApproval,
      update: enUpdate,
      chrome: enChrome,
      app: enApp,
    },
    zh: {
      common: zhCommon,
      chat: zhChat,
      settings: zhSettings,
      kb: zhKb,
      marketplace: zhMarketplace,
      workspace: zhWorkspace,
      onboarding: zhOnboarding,
      ask: zhAsk,
      approval: zhApproval,
      update: zhUpdate,
      chrome: zhChrome,
      app: zhApp,
    },
  },
  lng: detectLang(),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});

/** Switch language and remember the choice for future sessions.
 *  `"system"` clears the stored override so detection follows the OS. */
export function setAppLang(lang: AppLang | "system"): void {
  try {
    if (lang === "system") localStorage.removeItem(LANG_STORAGE_KEY);
    else localStorage.setItem(LANG_STORAGE_KEY, lang);
  } catch {
    /* ignore */
  }
  void i18n.changeLanguage(lang === "system" ? detectLang() : lang);
}

/** Current app language, or `"system"` when no explicit override is stored. */
export function currentLangChoice(): AppLang | "system" {
  try {
    const stored = localStorage.getItem(LANG_STORAGE_KEY);
    if (stored === "en" || stored === "zh") return stored;
  } catch {
    /* ignore */
  }
  return "system";
}

export default i18n;
