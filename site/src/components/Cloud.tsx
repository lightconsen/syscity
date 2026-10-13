import { ArrowRight, Cloud, RefreshCw, Store } from "lucide-react";
import { cloudUrl, useLanguage } from "../i18n";

const PLANS = [
  { key: "free", priceKey: "free" },
  { key: "pro", priceKey: "pro" },
  { key: "business", priceKey: "business" },
] as const;

/** "Syscity Cloud" product section (after Platforms): cloud LLM/search, device
 * sync, and the connector/expert marketplace, plus the plan tiers, with a CTA
 * to the console. */
export default function CloudSection() {
  const { t, lang } = useLanguage();

  return (
    <section id="cloud" className="border-y border-line bg-page">
      <div className="mx-auto max-w-6xl px-6 py-28">
        <div className="mb-14 max-w-2xl">
          <h2 className="text-4xl font-black tracking-tight sm:text-5xl">
            {t.cloud.eyebrow}
          </h2>
          <p className="mt-2 text-lg text-muted">{t.cloud.titleA} {t.cloud.titleB}</p>
          <p className="mt-4 text-muted">{t.cloud.lead}</p>
        </div>

        <div className="grid gap-5 sm:grid-cols-2 lg:grid-cols-3">
          <article className="card rounded-xl p-6 transition hover:border-brand-500/40">
            <div className="mb-4 inline-flex h-10 w-10 items-center justify-center rounded-lg bg-brand-500/10 text-brand-500">
              <Cloud className="h-5 w-5" />
            </div>
            <h3 className="text-base font-bold">{t.cloud.card1Title}</h3>
            <p className="mt-1.5 text-sm text-muted">{t.cloud.card1Desc}</p>
          </article>
          <article className="card rounded-xl p-6 transition hover:border-brand-500/40">
            <div className="mb-4 inline-flex h-10 w-10 items-center justify-center rounded-lg bg-brand-500/10 text-brand-500">
              <RefreshCw className="h-5 w-5" />
            </div>
            <h3 className="text-base font-bold">{t.cloud.card2Title}</h3>
            <p className="mt-1.5 text-sm text-muted">{t.cloud.card2Desc}</p>
          </article>
          <article className="card rounded-xl p-6 transition hover:border-brand-500/40">
            <div className="mb-4 inline-flex h-10 w-10 items-center justify-center rounded-lg bg-brand-500/10 text-brand-500">
              <Store className="h-5 w-5" />
            </div>
            <h3 className="text-base font-bold">{t.cloud.card3Title}</h3>
            <p className="mt-1.5 text-sm text-muted">{t.cloud.card3Desc}</p>
          </article>
        </div>

        {/* Plans */}
        <div className="mt-16">
          <p className="text-center text-xs font-semibold uppercase tracking-[0.18em] text-faint">
            {t.cloud.plansEyebrow}
          </p>
          <h3 className="mt-3 text-center text-2xl font-bold">{t.cloud.plansTitle}</h3>
          <p className="mx-auto mt-3 max-w-xl text-center text-sm text-muted">
            {t.cloud.plansSubtitle}
          </p>
          <div className="mt-10 grid gap-4 md:grid-cols-3">
            {PLANS.map((p) => {
              const highlight = p.key === "pro";
              return (
                <div
                  key={p.key}
                  className={`card flex flex-col rounded-xl p-6 ${
                    highlight ? "border-brand-500/50 shadow-[0_12px_32px_rgba(178,42,194,0.12)]" : ""
                  }`}
                >
                  {highlight && (
                    <span className="mb-3 inline-flex w-fit rounded-full bg-brand-500/10 px-2.5 py-0.5 text-[11px] font-semibold text-brand-600">
                      {t.cloud.recommended}
                    </span>
                  )}
                  <p className="text-sm font-bold">{t.cloud[`plan_${p.key}_name` as keyof typeof t.cloud]}</p>
                  <p className="mt-2 text-3xl font-black">
                    {t.cloud[`plan_${p.key}_price` as keyof typeof t.cloud]}
                  </p>
                  <p className="mt-1 text-sm text-muted">
                    {t.cloud[`plan_${p.key}_credits` as keyof typeof t.cloud]}
                  </p>
                  <ul className="mt-4 space-y-1.5 text-sm text-muted">
                    {(t.cloud[`plan_${p.key}_features` as keyof typeof t.cloud] as string[]).map((f, i) => (
                      <li key={i} className="flex items-start gap-2">
                        <span className="mt-1 h-1.5 w-1.5 shrink-0 rounded-full bg-brand-500" />
                        {f}
                      </li>
                    ))}
                  </ul>
                </div>
              );
            })}
          </div>
          <p className="mx-auto mt-5 max-w-xl text-center text-xs text-faint">
            {t.cloud.plansNote}
          </p>
        </div>

        <div className="mt-12 text-center">
          <a
            href={cloudUrl(lang)}
            target="_blank"
            rel="noreferrer"
            className="inline-flex h-12 items-center gap-2 rounded-md bg-brand-500 px-6 text-[15px] font-semibold text-white shadow-[0_8px_24px_rgba(178,42,194,0.3)] transition hover:bg-brand-600"
          >
            {t.cloud.cta}
            <ArrowRight className="h-4 w-4" />
          </a>
        </div>
      </div>
    </section>
  );
}
