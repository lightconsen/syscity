import { Star, Languages } from "lucide-react";
import { cloudUrl, useLanguage } from "../i18n";

import syscityLogo from "../assets/syscity.webp";

/** The logo's encoded size (192×172); the CSS sets the displayed size, these
 * reserve the box so the header does not shift when it decodes. */
const LOGO_W = 192;
const LOGO_H = 172;

export default function Nav() {
  const { lang, setLang, t } = useLanguage();

  const LINKS = [
    { href: "#features", label: t.nav.features },
    { href: "#download", label: t.nav.platforms },
    { href: "#quickstart", label: t.nav.quickstart },
  ];

  return (
    <header className="sticky top-0 z-50 border-b border-line bg-page/80 backdrop-blur-md">
      <nav className="mx-auto flex h-16 max-w-6xl items-center justify-between px-6">
        <a href="#top" className="flex items-center gap-2.5">
          <img
            src={syscityLogo}
            alt="Syscity logo"
            width={LOGO_W}
            height={LOGO_H}
            className="h-8 w-8 rounded-md object-contain"
          />
          <span className="text-[15px] font-bold tracking-tight">Syscity</span>
        </a>

        <div className="hidden items-center gap-8 text-sm font-medium text-muted md:flex">
          {LINKS.map((l) => (
            <a key={l.href} href={l.href} className="transition hover:text-ink">
              {l.label}
            </a>
          ))}
        </div>

        <div className="flex items-center gap-2.5">
          <button
            onClick={() => setLang(lang === "en" ? "zh" : "en")}
            aria-label={lang === "en" ? "切换到中文" : "Switch to English"}
            className="inline-flex items-center gap-1.5 rounded-md border border-line bg-panel px-3 py-2 text-sm font-semibold text-muted transition hover:border-brand-500/60 hover:text-brand-600"
          >
            <Languages className="h-4 w-4" />
            {t.nav.switchTo}
          </button>
          <a
            href="https://github.com/lightconsen/syscity"
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1.5 rounded-md border border-line bg-panel px-3.5 py-2 text-sm font-semibold transition hover:border-brand-500/60 hover:text-brand-600"
          >
            <Star className="h-4 w-4" />
            <span className="hidden sm:inline">{t.nav.starLong}</span>
            <span className="sm:hidden">{t.nav.starShort}</span>
          </a>
          <a
            href={cloudUrl(lang)}
            target="_blank"
            rel="noreferrer"
            className="hidden rounded-md bg-brand-500 px-3.5 py-2 text-sm font-semibold text-white transition hover:bg-brand-600 sm:inline-flex"
          >
            {t.nav.cloud}
          </a>
        </div>
      </nav>
    </header>
  );
}
