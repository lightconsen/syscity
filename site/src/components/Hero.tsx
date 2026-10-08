import { useState } from "react";
import { Check, Copy, ArrowRight } from "lucide-react";
import GithubMark from "./GithubMark";
import { useLanguage } from "../i18n";

import syscityLogo from "../assets/syscity.webp";

/** The logo's encoded size (192×172); the CSS sets the displayed size, these
 * reserve the box so the header does not shift when it decodes. */
const LOGO_W = 192;
const LOGO_H = 172;

const INSTALL_CMD_UNIX = "curl -sSL https://syscity.net/install.sh | bash";
const INSTALL_CMD_WINDOWS = "irm https://syscity.net/install.ps1 | iex";

function InstallChip() {
  const [copied, setCopied] = useState(false);
  const [tab, setTab] = useState<"unix" | "windows">("unix");
  const { t } = useLanguage();

  const cmd = tab === "unix" ? INSTALL_CMD_UNIX : INSTALL_CMD_WINDOWS;

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(cmd);
      setCopied(true);
      setTimeout(() => setCopied(false), 1800);
    } catch {
      /* clipboard unavailable — ignore */
    }
  };

  return (
    <div className="mx-auto max-w-xl space-y-3">
      <div className="flex justify-center gap-4">
        <button
          onClick={() => setTab("unix")}
          className={`px-2 py-1 text-xs font-semibold transition ${
            tab === "unix"
              ? "text-brand-500 border-b-2 border-brand-500"
              : "text-muted hover:text-ink"
          }`}
        >
          macOS / Linux
        </button>
        <button
          onClick={() => setTab("windows")}
          className={`px-2 py-1 text-xs font-semibold transition ${
            tab === "windows"
              ? "text-brand-500 border-b-2 border-brand-500"
              : "text-muted hover:text-ink"
          }`}
        >
          Windows
        </button>
      </div>
      <div className="flex items-center gap-3 rounded-lg border border-line bg-panel-2 px-4 py-3 font-mono text-[13px] text-muted">
        <span className="font-semibold text-brand-500">
          {tab === "unix" ? "$" : "PS>"}
        </span>
        <span className="flex-1 truncate text-left text-ink/90">{cmd}</span>
        <button
          onClick={copy}
          aria-label={t.hero.copyInstall}
          className="shrink-0 rounded-md p-1.5 text-faint transition hover:bg-line hover:text-ink"
        >
          {copied ? <Check className="h-4 w-4 text-brand-500" /> : <Copy className="h-4 w-4" />}
        </button>
      </div>
    </div>
  );
}

export default function Hero() {
  const { lang, t } = useLanguage();

  return (
    <section id="top" className="relative overflow-hidden">
      <div className="hero-glow pointer-events-none absolute inset-0" aria-hidden="true" />

      <div className="relative mx-auto flex max-w-4xl flex-col items-center px-6 pb-20 pt-20 text-center sm:pt-28">
        <div className="animate-fade-up mb-8">
          <div className="animate-float">
            <img
              src={syscityLogo}
              alt="Syscity logo"
              width={LOGO_W}
              height={LOGO_H}
              className="h-20 w-20 rounded-2xl object-contain shadow-[0_12px_32px_rgba(178,42,194,0.25)]"
            />
          </div>
        </div>

        <p
          className={`animate-fade-up mb-6 inline-flex items-center gap-2 rounded-full border border-brand-500/25 bg-brand-500/5 px-4 py-1.5 text-xs font-semibold text-brand-600 [animation-delay:80ms] ${
            lang === "zh" ? "tracking-[0.12em]" : "uppercase tracking-[0.18em]"
          }`}
        >
          {t.hero.badge}
        </p>

        <h1 className="animate-fade-up text-[clamp(2.75rem,7vw,4.75rem)] font-black leading-[1.05] tracking-tight [animation-delay:160ms]">
          {t.hero.titleTop}
          <br />
          <span className="text-gradient">{t.hero.titleBottom}</span>
        </h1>

        <p className="animate-fade-up mt-6 max-w-2xl text-base leading-relaxed text-muted sm:text-lg [animation-delay:240ms]">
          {t.hero.subtitle}
        </p>

        <div className="animate-fade-up mt-10 flex flex-wrap items-center justify-center gap-4 [animation-delay:320ms]">
          <a
            href="#quickstart"
            className="inline-flex h-12 items-center gap-2 rounded-md bg-brand-500 px-6 text-[15px] font-semibold text-white shadow-[0_8px_24px_rgba(178,42,194,0.3)] transition hover:bg-brand-600"
          >
            {t.hero.getStarted}
            <ArrowRight className="h-4 w-4" />
          </a>
          <a
            href="https://github.com/lightconsen/syscity"
            target="_blank"
            rel="noreferrer"
            className="inline-flex h-12 items-center gap-2 rounded-md border border-brand-500/60 px-6 text-[15px] font-semibold text-brand-600 transition hover:border-brand-500 hover:bg-brand-500/5"
          >
            <GithubMark className="h-4 w-4" />
            {t.hero.viewOnGithub}
          </a>
        </div>

        <div className="animate-fade-up mt-12 w-full [animation-delay:400ms]">
          <InstallChip />
        </div>
      </div>
    </section>
  );
}
