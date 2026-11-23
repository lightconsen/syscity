import { useEffect, useRef, useState } from "react";
import { useLanguage } from "../i18n";

const base = import.meta.env.BASE_URL;

const DESKTOP = [
  { name: "Windows", icon: `${base}assets/platforms/window.svg`, url: "https://syscity.net/releases/syscity-desktop-windows-amd64.exe" },
  { name: "Linux", icon: `${base}assets/platforms/linux.svg`, url: "https://syscity.net/releases/syscity-desktop-linux-amd64.AppImage" },
];

/**
 * Mobile clients.
 *
 * iOS is deliberately absent: the app has no App Store listing yet, and the
 * URL this card used (`apps.apple.com/app/syscity`, no numeric id) does not
 * 404 — Apple answers it with a store landing page, at one point "Today".
 * Shipping that link sends a visitor who wants Syscity somewhere else
 * entirely. Restore the entry with the real `.../app/<slug>/id<digits>` URL
 * once the listing is live; `lookup?bundleId=net.syscity` is how to confirm
 * it.
 */
const MOBILE = [
  { name: "Android", icon: `${base}assets/platforms/android.svg`, url: "https://play.google.com/store/apps/details?id=net.syscity" },
];

const MAC_VARIANTS = [
  { name: "Apple Silicon", url: "https://syscity.net/releases/syscity-desktop-macos-arm64.dmg" },
  { name: "Intel", url: "https://syscity.net/releases/syscity-desktop-macos-amd64.dmg" },
];

/** Download section: the merged Platforms + Download area — per-platform
 * clients, with desktop builds as direct downloads and mobile linking to the
 * app stores. macOS shows a chooser (Apple Silicon / Intel) on click. */
export default function Download() {
  const { t } = useLanguage();
  const [macOpen, setMacOpen] = useState(false);
  const macRef = useRef<HTMLDivElement>(null);

  // Close the macOS chooser when clicking outside or moving the pointer away.
  useEffect(() => {
    if (!macOpen) return;
    const onPointerDown = (e: PointerEvent) => {
      if (macRef.current && !macRef.current.contains(e.target as Node)) {
        setMacOpen(false);
      }
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [macOpen]);

  const cardCls =
    "card inline-flex items-center justify-center rounded-xl p-3 transition hover:border-brand-500/40 hover:shadow-[0_8px_24px_rgba(178,42,194,0.10)]";

  return (
    <section id="download" className="border-y border-line bg-alt">
      <div className="mx-auto max-w-6xl px-6 py-28">
        <div className="mb-14 text-center">
          <h2 className="text-3xl font-black tracking-tight sm:text-4xl">
            {t.download.title}
          </h2>
          <p className="mt-4 text-xl font-bold text-brand-600">
            {t.download.subtitle}
          </p>
          <p className="mx-auto mt-4 max-w-2xl text-muted">{t.download.lead}</p>
        </div>

        {/* One row of platform cards. With iOS gone, desktop + Android is
            four cards, which fits a phone at 48 px icons — so they share a
            wrapping row instead of Android sitting alone on a line of its
            own. It is still labelled "every device, one agent" above. */}
        <div className="flex flex-wrap justify-center gap-3">
          {/* macOS: single icon, click to pick Apple Silicon / Intel */}
          <div ref={macRef} className="relative">
            <button
              type="button"
              title="macOS"
              aria-expanded={macOpen}
              onClick={() => setMacOpen((v) => !v)}
              className={cardCls}
            >
              <img src={`${base}assets/platforms/mac.svg`} alt="macOS" className="h-12 w-12" loading="lazy" />
            </button>
            {macOpen && (
              <div className="absolute left-1/2 top-full z-10 mt-2 -translate-x-1/2 rounded-xl border border-line bg-panel p-2 shadow-lg">
                {MAC_VARIANTS.map((v) => (
                  <a
                    key={v.name}
                    href={v.url}
                    target="_blank"
                    rel="noreferrer"
                    className="block whitespace-nowrap rounded-lg px-4 py-2 text-sm font-medium text-ink transition hover:bg-black/5 dark:hover:bg-white/5"
                  >
                    {v.name}
                  </a>
                ))}
              </div>
            )}
          </div>

          {DESKTOP.map((p) => (
            <a key={p.name} href={p.url} target="_blank" rel="noreferrer" title={p.name} className={cardCls}>
              <img src={p.icon} alt={p.name} className="h-12 w-12" loading="lazy" />
            </a>
          ))}

          {MOBILE.map((p) => (
            <a key={p.name} href={p.url} target="_blank" rel="noreferrer" title={p.name} className={cardCls}>
              <img src={p.icon} alt={p.name} className="h-12 w-12" loading="lazy" />
            </a>
          ))}
        </div>

        <p className="mt-8 text-center text-sm text-muted">{t.download.cloudNote}</p>
      </div>
    </section>
  );
}
