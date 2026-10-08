import { MessageSquare, FileText } from "lucide-react";
import GithubMark from "./GithubMark";
import { cloudUrl, useLanguage } from "../i18n";

import syscityLogo from "../assets/syscity.webp";

/** The logo's encoded size (192×172); the CSS sets the displayed size, these
 * reserve the box so the header does not shift when it decodes. */
const LOGO_W = 192;
const LOGO_H = 172;

export default function Footer() {
  const { t, lang } = useLanguage();

  return (
    <footer className="border-t border-line bg-alt">
      <div className="mx-auto max-w-6xl px-6 py-14">
        <div className="grid gap-10 md:grid-cols-3">
          <div>
            <div className="flex items-center gap-2.5">
              <img
                src={syscityLogo}
                alt="Syscity logo"
                width={LOGO_W}
                height={LOGO_H}
                className="h-7 w-7 rounded-md object-contain"
              />
              <span className="text-sm font-bold">Syscity</span>
            </div>
            <p className="mt-3 max-w-xs text-sm text-muted">{t.footer.tagline}</p>
          </div>

          <div>
            <p className="mb-3 text-xs font-semibold uppercase tracking-wider text-faint">
              {t.footer.product}
            </p>
            <ul className="space-y-2 text-sm text-muted">
              <li><a href="#features" className="transition hover:text-ink">{t.footer.features}</a></li>
              <li><a href="#platforms" className="transition hover:text-ink">{t.footer.platforms}</a></li>
              <li><a href="#quickstart" className="transition hover:text-ink">{t.footer.quickstart}</a></li>
              <li>
                <a
                  href={cloudUrl(lang)}
                  target="_blank"
                  rel="noreferrer"
                  className="transition hover:text-ink"
                >
                  {t.footer.cloud}
                </a>
              </li>
            </ul>
          </div>

          <div>
            <p className="mb-3 text-xs font-semibold uppercase tracking-wider text-faint">
              {t.footer.community}
            </p>
            <ul className="space-y-2 text-sm text-muted">
              <li>
                <a
                  href="https://github.com/lightconsen/syscity"
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center gap-2 transition hover:text-ink"
                >
                  <GithubMark className="h-4 w-4" /> GitHub
                </a>
              </li>
              <li>
                <a
                  href="https://discord.gg/aaXghvzD"
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center gap-2 transition hover:text-ink"
                >
                  <MessageSquare className="h-4 w-4" /> Discord
                </a>
              </li>
              <li>
                <a
                  href="https://github.com/lightconsen/syscity/tree/main/docs"
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center gap-2 transition hover:text-ink"
                >
                  <FileText className="h-4 w-4" /> {t.footer.documentation}
                </a>
              </li>
            </ul>
          </div>
        </div>

        <div className="mt-12 flex flex-col items-center justify-between gap-3 border-t border-line pt-6 text-xs text-faint sm:flex-row">
          <p>© {new Date().getFullYear()} Syscity</p>
          <p>{t.footer.license}</p>
        </div>
      </div>
    </footer>
  );
}
