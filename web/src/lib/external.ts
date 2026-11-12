// Opening a URL in the user's browser.
//
// `window.open` does nothing under the Tauri WebView: WKWebView refuses a new
// window unless the host registered an `on_new_window` handler, and this app
// registers none. The call therefore returns a window whose navigation never
// happens — silently. For the cloud sign-in button that was the whole bug:
// the OAuth flow never started, so nothing ever came back to detect, and the
// button sat on its spinner until the three-minute timeout.
//
// The host's `shell` plugin already exposes an `open` command; its scope is
// narrowed to `http(s)` in `desktop/tauri.conf.json` (`plugins > shell > open`),
// so this can hand over a URL but not an arbitrary path or program.
//
// In a browser (the gateway serving the SPA directly) `window.open` is the
// right call and is kept — the popup's `postMessage` back to the opener is
// part of the login flow there.

/** True when running inside the desktop/mobile Tauri shell. */
function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI__" in window;
}

/**
 * Hand `url` to the system browser, or open a tab when running in a browser.
 *
 * Returns `false` only in the Tauri case, where it must not fail silently:
 * the caller is usually waiting for something to happen as a result.
 */
export async function openExternal(url: string): Promise<boolean> {
  if (!inTauri()) {
    window.open(url, "_blank");
    return true;
  }
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    // `shell|open` takes `{ path, with }`; `with: null` means "system default".
    await invoke("plugin:shell|open", { path: url, with: null });
    return true;
  } catch (e) {
    console.error("could not open the system browser:", e);
    return false;
  }
}

/**
 * Route `target="_blank"` links through {@link openExternal} in the desktop
 * shell.
 *
 * Anchors are the other half of the same WebView limitation: the console's
 * "manage plan", the recharge link on a low-balance banner, the links injected
 * into an assistant message — all plain `<a target="_blank">`, and all dead
 * under the shell for exactly the reason `window.open` is. One listener covers
 * every one of them, including the ones rendered from markdown, so they do not
 * each have to become a button.
 *
 * Does nothing in a browser, where the anchor already works.
 */
export function installExternalLinkHandler(): void {
  if (!inTauri() || typeof document === "undefined") return;
  document.addEventListener(
    "click",
    (event) => {
      if (event.defaultPrevented || event.button !== 0) return;
      // Modifier-clicks are included: in the shell there is no "open in new
      // tab" to distinguish them from, and letting the anchor's default run
      // would land the click back in the no-op.
      const anchor = (event.target as Element | null)?.closest?.("a[target='_blank']");
      const href = anchor?.getAttribute("href");
      if (!href || !/^https?:\/\//i.test(href)) return;
      event.preventDefault();
      event.stopPropagation();
      void openExternal(href);
    },
    // Capture: the SPA's own handlers must not see this click and navigate the
    // main WebView away from the app.
    true,
  );
}
