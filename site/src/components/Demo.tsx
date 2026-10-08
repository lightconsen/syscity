import { useEffect, useState } from "react";
import { useLanguage } from "../i18n";

import demoDarkMp4 from "../assets/demo-dark.mp4";
import demoDarkPoster from "../assets/demo-dark-poster.webp";
import demoDarkWebm from "../assets/demo-dark.webm";
import demoLightMp4 from "../assets/demo-light.mp4";
import demoLightPoster from "../assets/demo-light-poster.webp";
import demoLightWebm from "../assets/demo-light.webm";
import androidDarkAvif from "../assets/mobile-android-dark.avif";
import androidDarkWebp from "../assets/mobile-android-dark.webp";
import androidLightAvif from "../assets/mobile-android-light.avif";
import androidLightWebp from "../assets/mobile-android-light.webp";
import iosDarkAvif from "../assets/mobile-ios-dark.avif";
import iosDarkWebp from "../assets/mobile-ios-dark.webp";
import iosLightAvif from "../assets/mobile-ios-light.avif";
import iosLightWebp from "../assets/mobile-ios-light.webp";

/** The demo animation's encoded size (1200×567 rounded to even height). */
const DEMO_W = 1200;
const DEMO_H = 566;

/** The phone screenshots' encoded size. */
const SHOT_W = 480;
const SHOT_H = 1040;

/**
 * Whether the visitor prefers a dark interface.
 *
 * `<picture>` picks between the two screenshot sets by itself — but a
 * `<video>` cannot: its `<source media>` attribute is not honoured, so the
 * choice has to come from script. Set once on mount and kept in step with the
 * OS setting.
 */
function usePrefersDark(): boolean {
  // Starts `false` rather than reading the query during render, for the same
  // reason the language does: the HTML is prerendered light, and a first
  // client render that claimed dark would disagree with it — React answers
  // that by discarding the prerendered subtree. Resolved in the effect below,
  // so a dark visitor gets the dark clip one frame later.
  const [dark, setDark] = useState(false);

  useEffect(() => {
    const query = window.matchMedia("(prefers-color-scheme: dark)");
    setDark(query.matches);
    const onChange = (event: MediaQueryListEvent) => setDark(event.matches);
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  }, []);

  return dark;
}

/**
 * Play `video` once it scrolls into view, and never for a visitor who asked
 * for reduced motion.
 *
 * With `autoPlay` the browser fetches the whole clip on page load even though
 * the demo sits below the fold — 73 KB for something most visitors have not
 * scrolled to. Waiting for the element to be seen means a visitor who never
 * scrolls never pays for it, and the poster (already loaded, 22 KB) is what
 * they see instead; the poster also carries the reserved box, so starting
 * playback moves nothing. The margin is zero on purpose: any prefetch margin
 * just moves the line at which the download starts, and this clip is not worth
 * one.
 */
function usePlayInView(video: HTMLVideoElement | null) {
  useEffect(() => {
    if (!video) return;
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;

    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) {
            // A rejected play() means the browser declined autoplay (e.g. no
            // muted attribute support); the poster stays, which is a complete
            // rendering of the section either way.
            void video.play().catch(() => {});
            observer.disconnect();
          }
        }
      },
      { rootMargin: "0px" },
    );
    observer.observe(video);
    return () => observer.disconnect();
  }, [video]);
}

export default function Demo() {
  const { t } = useLanguage();
  const dark = usePrefersDark();
  // The element lands in state (not a ref) because the `<video>` is remounted
  // with a new `key` when the theme resolves — remounting is what makes the
  // browser re-read the `<source>` children, and the observer has to follow
  // the new element.
  const [video, setVideo] = useState<HTMLVideoElement | null>(null);
  usePlayInView(video);

  return (
    <section className="mx-auto max-w-5xl px-6 pb-24">
      <div className="card overflow-hidden rounded-2xl shadow-[0_24px_64px_rgba(25,26,35,0.10)]">
        <div className="flex items-center gap-2 border-b border-line bg-panel-2 px-4 py-3">
          <span className="h-3 w-3 rounded-full bg-[#ff5f57]" aria-hidden="true" />
          <span className="h-3 w-3 rounded-full bg-[#febc2e]" aria-hidden="true" />
          <span className="h-3 w-3 rounded-full bg-[#28c840]" aria-hidden="true" />
          <span className="ml-3 text-xs text-faint">{t.demo.chromeTitle}</span>
        </div>
        {/*
          The demo used to be a 195 KB GIF — 87% of the page's weight, and the
          largest single cause of the layout shift. A video codec expresses
          inter-frame change the way a screen recording actually changes, so
          the same clip costs a third of the bytes; the poster fills the frame
          before playback, and width/height reserve the space so nothing moves
          underneath it. Muted because the clip has no audio and browsers only
          autoplay muted video.
        */}
        <video
          key={dark ? "dark" : "light"}
          ref={setVideo}
          className="w-full"
          width={DEMO_W}
          height={DEMO_H}
          poster={dark ? demoDarkPoster : demoLightPoster}
          loop
          muted
          playsInline
          preload="none"
          aria-label={t.demo.alt}
        >
          <source src={dark ? demoDarkWebm : demoLightWebm} type="video/webm" />
          <source src={dark ? demoDarkMp4 : demoLightMp4} type="video/mp4" />
        </video>
      </div>
      <p className="mt-4 text-center text-sm text-muted">
        {t.demo.captionBefore} <code className="font-mono">{t.demo.captionTool}</code>
        {t.demo.captionAfter}
      </p>

      {/* Mobile app screenshots (iOS + Android), below the demo animation.
          Each visitor downloads one theme's set: the dark sources are
          media-scoped, so a dark-mode browser never fetches the light pair. */}
      <div className="mt-10 flex justify-center gap-4">
        <figure className="flex flex-col items-center">
          <picture>
            <source media="(prefers-color-scheme: dark)" type="image/avif" srcSet={iosDarkAvif} />
            <source media="(prefers-color-scheme: dark)" type="image/webp" srcSet={iosDarkWebp} />
            <source type="image/avif" srcSet={iosLightAvif} />
            <source type="image/webp" srcSet={iosLightWebp} />
            <img
              src={iosLightWebp}
              alt="Syscity on iOS"
              width={SHOT_W}
              height={SHOT_H}
              className="card h-[560px] w-auto rounded-2xl object-contain"
              loading="lazy"
              decoding="async"
            />
          </picture>
          <figcaption className="mt-2 text-sm text-muted">{t.demo.iosCaption}</figcaption>
        </figure>
        <figure className="flex flex-col items-center">
          <picture>
            <source
              media="(prefers-color-scheme: dark)"
              type="image/avif"
              srcSet={androidDarkAvif}
            />
            <source media="(prefers-color-scheme: dark)" type="image/webp" srcSet={androidDarkWebp} />
            <source type="image/avif" srcSet={androidLightAvif} />
            <source type="image/webp" srcSet={androidLightWebp} />
            <img
              src={androidLightWebp}
              alt="Syscity on Android"
              width={SHOT_W}
              height={SHOT_H}
              className="card h-[560px] w-auto rounded-2xl object-contain"
              loading="lazy"
              decoding="async"
            />
          </picture>
          <figcaption className="mt-2 text-sm text-muted">{t.demo.androidCaption}</figcaption>
        </figure>
      </div>
    </section>
  );
}
