import { Terminal } from "lucide-react";
import { useLanguage } from "../i18n";

export default function QuickStart() {
  const { t } = useLanguage();

  const LINES: { prompt?: string; text: string; dim?: boolean }[] = [
    { prompt: "$", text: "curl -sSL https://syscity.net/install.sh | bash" },
    { prompt: "✓", text: t.quickstart.termInstalled, dim: true },
    { prompt: "$", text: "syscity setup" },
    { prompt: "✓", text: t.quickstart.termConfigured, dim: true },
    { prompt: "$", text: "syscity start" },
    { prompt: "🚀", text: t.quickstart.termRunning, dim: true },
  ];

  return (
    <section id="quickstart">
      <div className="mx-auto max-w-6xl px-6 py-28">
        <div className="grid items-center gap-12 lg:grid-cols-2">
          <div>
            <p className="mb-3 text-xs font-semibold uppercase tracking-[0.18em] text-brand-500">
              {t.quickstart.eyebrow}
            </p>
            <h2 className="text-3xl font-black tracking-tight sm:text-4xl">
              {t.quickstart.titleA}{" "}
              <span className="text-gradient">{t.quickstart.titleB}</span>
            </h2>
            <p className="mt-4 max-w-md text-muted">{t.quickstart.lead}</p>
            <div className="mt-8 flex flex-wrap gap-4">
              <a
                href="https://github.com/lightconsen/syscity#readme"
                target="_blank"
                rel="noreferrer"
                className="inline-flex h-12 items-center gap-2 rounded-md bg-brand-500 px-6 text-[15px] font-semibold text-white shadow-[0_8px_24px_rgba(178,42,194,0.3)] transition hover:bg-brand-600"
              >
                {t.quickstart.readDocs}
              </a>
              <a
                href="https://github.com/lightconsen/syscity#quick-start"
                target="_blank"
                rel="noreferrer"
                className="inline-flex h-12 items-center gap-2 rounded-md border border-brand-500/60 px-6 text-[15px] font-semibold text-brand-600 transition hover:border-brand-500 hover:bg-brand-500/5"
              >
                {t.quickstart.githubReadme}
              </a>
            </div>
          </div>

          <div className="overflow-hidden rounded-xl bg-[#191a23] shadow-[0_24px_64px_rgba(25,26,35,0.18)]">
            <div className="flex items-center gap-2 border-b border-white/10 bg-white/5 px-4 py-3">
              <span className="h-3 w-3 rounded-full bg-[#ff5f57]" aria-hidden="true" />
              <span className="h-3 w-3 rounded-full bg-[#febc2e]" aria-hidden="true" />
              <span className="h-3 w-3 rounded-full bg-[#28c840]" aria-hidden="true" />
              <span className="ml-3 inline-flex items-center gap-1.5 text-xs text-white/55">
                <Terminal className="h-3.5 w-3.5" />
                syscity
              </span>
            </div>
            <div className="space-y-2.5 px-5 py-5 font-mono text-[13px] leading-relaxed">
              {LINES.map((l, i) => (
                <div
                  key={i}
                  className={`flex items-baseline gap-2 ${l.dim ? "text-white/55" : "text-white/90"}`}
                >
                  <span className={l.prompt === "$" ? "text-brand-400" : "text-primary-300"}>
                    {l.prompt}
                  </span>
                  <span className="break-all">{l.text}</span>
                </div>
              ))}
            </div>
          </div>
        </div>
      </div>
    </section>
  );
}
