import { useEffect, useRef, useState } from "react";
import { Maximize2, Minimize2 } from "lucide-react";
import { useLanguage } from "../i18n";

import demoMp4 from "../assets/demo-desktop.mp4";
import demoPoster from "../assets/demo-desktop-poster.webp";
import demoWebm from "../assets/demo-desktop.webm";
import androidDarkAvif from "../assets/mobile-android-dark.avif";
import androidDarkWebp from "../assets/mobile-android-dark.webp";
import androidLightAvif from "../assets/mobile-android-light.avif";
import androidLightWebp from "../assets/mobile-android-light.webp";
import iosDarkAvif from "../assets/mobile-ios-dark.avif";
import iosDarkWebp from "../assets/mobile-ios-dark.webp";
import iosLightAvif from "../assets/mobile-ios-light.avif";
import iosLightWebp from "../assets/mobile-ios-light.webp";

/** The demo clip's encoded size. It is a composite: the app and the Linux
 * desktop it drove, side by side, with a caption band along the bottom, so the
 * frame is wider and taller than a plain 1200×566 screen recording. `Demo.tsx`
 * reserves exactly this box before the clip decodes, so the poster and the
 * video are the same shape. */
const DEMO_W = 1600;
const DEMO_H = 662;

/** The phone screenshots' encoded size. The Android capture is 26 px taller
 * than the iOS one, so the two are declared separately — a shared height
 * would reserve the wrong box for one of them and shift the layout when it
 * decoded. */
const SHOT_W = 480;
const IOS_SHOT_H = 1040;
const ANDROID_SHOT_H = 1066;

/**
 * Play `video` once it scrolls into view, and never for a visitor who asked
 * for reduced motion.
 *
 * With `autoPlay` the browser fetches the whole clip on page load even though
 * the demo sits below the fold — 386 KB for something most visitors have not
 * scrolled to. Waiting for the element to be seen means a visitor who never
 * scrolls never pays for it, and the poster (already loaded, 50 KB) is what
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

/**
 * Keep `isFullscreen` in sync with the browser, not with the click.
 *
 * The Fullscreen API can be left without our button: Escape, the browser's own
 * overlay, or the page navigating away. Driving the icon from
 * `fullscreenchange` means the button never claims a state the element is not
 * in — and `document.fullscreenElement` is also what decides whether the next
 * click should ask to enter or to leave.
 */
function useFullscreenState(
  element: React.RefObject<HTMLDivElement | null>,
  setActive: (active: boolean) => void,
) {
  useEffect(() => {
    const onChange = () => setActive(document.fullscreenElement === element.current);
    document.addEventListener("fullscreenchange", onChange);
    return () => document.removeEventListener("fullscreenchange", onChange);
  }, [element, setActive]);
}

/**
 * Enter/leave fullscreen for the demo frame.
 *
 * A rejected request is left silent on purpose: browsers refuse without a user
 * gesture (and iOS refuses on a non-`<video>` element), and the clip stays
 * playable in place. The button is hidden where the API does not exist so it
 * cannot present a control that does nothing.
 *
 * `aria-pressed` is the state; the label says what the button will do, so it
 * changes with the state rather than staying "fullscreen" while already full.
 */
function FullscreenButton({
  isFullscreen,
  frameRef,
}: {
  isFullscreen: boolean;
  frameRef: React.RefObject<HTMLDivElement | null>;
}) {
  const { t } = useLanguage();
  const [supported, setSupported] = useState(false);

  // Prerendering runs without a document, so this cannot be a render-time
  // check — and a button that appears after hydration is better than one that
  // throws during the build.
  useEffect(() => {
    const el = frameRef.current;
    setSupported(Boolean(el && (el.requestFullscreen || (el as FullscreenCapable).webkitRequestFullscreen)));
  }, [frameRef]);

  if (!supported) return null;

  const toggle = () => {
    const el = frameRef.current;
    if (!el) return;
    if (document.fullscreenElement === el) {
      void document.exitFullscreen().catch(() => {});
    } else {
      const req =
        el.requestFullscreen?.bind(el) ??
        (el as FullscreenCapable).webkitRequestFullscreen?.bind(el);
      if (req) void Promise.resolve(req()).catch(() => {});
    }
  };

  return (
    <button
      type="button"
      onClick={toggle}
      aria-pressed={isFullscreen}
      aria-label={isFullscreen ? t.demo.exitFullscreen : t.demo.fullscreen}
      title={isFullscreen ? t.demo.exitFullscreen : t.demo.fullscreen}
      className="ml-auto inline-flex items-center gap-1.5 rounded-md border border-line bg-panel px-2.5 py-1.5 text-xs font-semibold text-muted transition hover:border-brand-500/60 hover:text-brand-600 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-500"
    >
      {isFullscreen ? (
        <Minimize2 className="h-3.5 w-3.5" aria-hidden="true" />
      ) : (
        <Maximize2 className="h-3.5 w-3.5" aria-hidden="true" />
      )}
      <span className="hidden sm:inline">
        {isFullscreen ? t.demo.exitFullscreen : t.demo.fullscreen}
      </span>
    </button>
  );
}

/** Safari exposes the Fullscreen API under the `webkit` prefix. */
type FullscreenCapable = {
  webkitRequestFullscreen?: () => Promise<void> | void;
};

export default function Demo() {
  const { t } = useLanguage();
  const [video, setVideo] = useState<HTMLVideoElement | null>(null);
  const frameRef = useRef<HTMLDivElement | null>(null);
  const [isFullscreen, setIsFullscreen] = useState(false);
  usePlayInView(video);
  useFullscreenState(frameRef, setIsFullscreen);

  return (
    <section className="mx-auto max-w-5xl px-6 pb-24">
      {/* The frame, not the <video>, is what goes fullscreen: the window bar
          and the control belong to the same box, so the clip does not end up
          letterboxed inside a bare fullscreen video element with the title bar
          missing. It also keeps the captured element identical to the one the
          layout already reserves space for. */}
      <div
        ref={frameRef}
        className="card demo-frame group relative overflow-hidden rounded-2xl shadow-[0_24px_64px_rgba(25,26,35,0.10)]"
      >
        <div className="demo-bar flex items-center gap-2 border-b border-line bg-panel-2 px-4 py-3">
          <span className="h-3 w-3 rounded-full bg-[#ff5f57]" aria-hidden="true" />
          <span className="h-3 w-3 rounded-full bg-[#febc2e]" aria-hidden="true" />
          <span className="h-3 w-3 rounded-full bg-[#28c840]" aria-hidden="true" />
          <span className="ml-3 text-xs text-faint">{t.demo.chromeTitle}</span>
          <FullscreenButton isFullscreen={isFullscreen} frameRef={frameRef} />
        </div>
        {/*
          The demo used to be a 195 KB GIF — 87% of the page's weight, and the
          largest single cause of the layout shift. A video codec expresses
          inter-frame change the way a screen recording actually changes, so
          the same clip costs a fraction of the bytes; the poster fills the
          frame before playback, and width/height reserve the space so nothing
          moves underneath it. Muted because the clip has no audio and browsers
          only autoplay muted video.

          One clip for both themes. It is a screen recording of the app doing
          real work on a Linux desktop, and re-staging it in dark mode would
          mean a second run on a metered model — the frames are evidence, not
          decoration. The surrounding page still follows the visitor's theme;
          only this window stays light, the way a screenshot would.
        */}
        <video
          ref={setVideo}
          className="w-full"
          width={DEMO_W}
          height={DEMO_H}
          poster={demoPoster}
          loop
          muted
          playsInline
          preload="none"
          aria-label={t.demo.alt}
        >
          <source src={demoWebm} type="video/webm" />
          <source src={demoMp4} type="video/mp4" />
        </video>
      </div>
      <p className="mt-4 text-center text-sm text-muted">
        {t.demo.captionBefore} <code className="font-mono">{t.demo.captionTool}</code>
        {t.demo.captionAfter}
      </p>

      {/* Mobile app screenshots (iOS + Android), below the demo animation.
          Each visitor downloads one theme's set: the dark sources are
          media-scoped, so a dark-mode browser never fetches the light pair. */}
      {/* Two 258 px-wide screenshots plus the gap need 532 px; a phone has
          ~327. Left as a non-wrapping flex row the pair was shrunk to 148/163
          px and `object-contain` letterboxed the capture inside a 560 px box
          — the screenshots read as tiny and half-empty. Wrapping (and
          refusing to shrink) puts them one above the other there, at full
          size, and keeps the side-by-side row on anything wider. */}
      <div className="mt-10 flex flex-wrap justify-center gap-4">
        <figure className="flex shrink-0 flex-col items-center">
          <picture>
            <source media="(prefers-color-scheme: dark)" type="image/avif" srcSet={iosDarkAvif} />
            <source media="(prefers-color-scheme: dark)" type="image/webp" srcSet={iosDarkWebp} />
            <source type="image/avif" srcSet={iosLightAvif} />
            <source type="image/webp" srcSet={iosLightWebp} />
            <img
              src={iosLightWebp}
              alt="Syscity on iOS"
              width={SHOT_W}
              height={IOS_SHOT_H}
              className="card h-[560px] w-auto rounded-2xl object-contain"
              loading="lazy"
              decoding="async"
            />
          </picture>
          <figcaption className="mt-2 text-sm text-muted">{t.demo.iosCaption}</figcaption>
        </figure>
        <figure className="flex shrink-0 flex-col items-center">
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
              height={ANDROID_SHOT_H}
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
