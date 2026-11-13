import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { CheckCircle2, Loader2, XCircle } from "lucide-react";
import { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { cloudSubmitToken } from "@/lib/cloud";

/**
 * The cloud OAuth return page (`/cloud/login/callback`).
 *
 * The console sends the browser back here with the session in the URL
 * fragment (`#token=…`), so this page is the one place the gateway can see
 * the token. It persists it over WS (`cloud.token`) and then has three
 * endings, chosen by where the sign-in started:
 *
 *  - **popup** (`window.opener` present, the browser's Account button): tell
 *    the opener and close. The app is already on the other side of the
 *    window, so leaving anything on screen would be an orphan tab.
 *  - **desktop** (`?flow=desktop`): the desktop shell hands the login URL to
 *    the system browser, so this tab has no opener and no app behind it.
 *    Navigating to the gateway's `/` here is what used to happen — and it is
 *    wrong twice: that page is a second, unauthenticated-ish copy of the app
 *    (the gateway's auth cookie is not there), and the person is left in a
 *    browser tab they never asked for. It shows a completion screen instead.
 *  - **tab** (the web app navigated itself here, welcome page flow) and the
 *    no-`flow` case (an older client, or a direct visit): going back to `/`
 *    really is the app, so that is what it does.
 *
 * Rendering a real screen rather than a redirect is also what makes the
 * popup case honest: a `window.close()` a browser refuses now leaves a page
 * that says the login worked, instead of the last frame of the OAuth hop.
 */
type Outcome =
  | { state: "working" }
  | { state: "done" }
  | { state: "failed"; message?: string };

export function CloudLoginCallback() {
  const { t } = useTranslation("chat");
  // The app's own transport is not mounted on this page (the app deliberately
  // does not render here), so the callback owns one. It is the same class, so
  // the WS ticket/auth handshake is unchanged.
  const transport = useMemo(() => new SyscityWebSocketTransport(), []);
  const [outcome, setOutcome] = useState<Outcome>({ state: "working" });
  /** Popup flow: we asked the window to close and it did not. */
  const [closeBlocked, setCloseBlocked] = useState(false);

  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const flow = params.get("flow");
    const isPopup = Boolean(window.opener);
    const token = new URLSearchParams(window.location.hash.slice(1)).get(
      "token",
    );

    // Where the completion screen belongs. A popup that closes itself never
    // shows it; everything else keeps the window it was given.
    const finish = () => {
      if (isPopup) {
        try {
          window.close();
        } catch {
          /* ignore */
        }
        // `close()` is a no-op for a window the script did not open (and
        // increasingly for any window a browser decides not to honour). If
        // we are still here a beat later, show the way out: close it.
        window.setTimeout(() => {
          if (!window.closed) {
            setCloseBlocked(true);
            setOutcome({ state: "done" });
          }
        }, 400);
        return;
      }
      if (flow === "desktop") {
        return setOutcome({ state: "done" });
      }
      // `tab`, or no `flow` at all: a client predating the flow parameter
      // navigated itself here, and `/` really is the app it came from.
      window.location.replace("/");
    };

    if (!token) {
      // No token: either the user cancelled at the provider, or something
      // stripped the fragment. Both end the flow — say so rather than
      // silently showing the app.
      finish();
      return;
    }

    void cloudSubmitToken(token, transport).then(({ ok, message }) => {
      if (ok) {
        try {
          localStorage.setItem("syscity_cloud_enabled_hint", "1");
        } catch {
          /* ignore */
        }
      }
      if (isPopup) {
        // Tell the opener the flow finished (it polls cloud.status as the
        // reliable fallback). targetOrigin "*": the opener may be on
        // 127.0.0.1 while this callback is on localhost — the receiver
        // validates the origin.
        try {
          window.opener.postMessage({ type: "syscity:login", ok, message }, "*");
        } catch {
          /* ignore */
        }
      }
      if (ok) {
        finish();
        return;
      }
      // Keep the page and say why: a network error from the gateway's own
      // call to the cloud API reads very differently from an expired token,
      // and the person has to know the login did not take.
      setOutcome({ state: "failed", message });
    });
    // Runs once, on mount: the token is in the URL, not in state.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [transport]);

  const tone =
    outcome.state === "failed"
      ? "text-red-600 dark:text-red-400"
      : "text-emerald-600 dark:text-emerald-400";
  const Icon =
    outcome.state === "failed" ? XCircle : outcome.state === "working" ? Loader2 : CheckCircle2;

  return (
    <div
      className="flex items-center justify-center bg-page text-primary"
      style={{ height: "100lvh" }}
      data-testid="cloud-login-result"
      data-state={outcome.state}
    >
      <div className="flex flex-col items-center text-center px-8 max-w-sm">
        <img
          src="/syscity.png"
          alt="Syscity"
          className="w-16 h-16 rounded-2xl shadow-sm mb-6"
          draggable={false}
        />
        <Icon
          className={`w-8 h-8 mb-4 ${outcome.state === "working" ? "animate-spin text-secondary" : tone}`}
        />
        <div className="text-lg font-semibold" data-testid="cloud-login-title">
          {outcome.state === "failed"
            ? t("CloudLogin.failed")
            : outcome.state === "done"
              ? t("CloudLogin.done")
              : t("CloudLogin.working")}
        </div>
        <div className="mt-1.5 text-sm text-secondary">
          {outcome.state === "failed"
            ? outcome.message || t("CloudLogin.failedHint")
            : outcome.state === "done"
              ? closeBlocked
                ? t("CloudLogin.closeHint")
                : t("CloudLogin.doneHint")
              : t("CloudLogin.workingHint")}
        </div>
        {(outcome.state === "done" || outcome.state === "failed") && (
          <button
            type="button"
            onClick={() => {
              try {
                window.close();
              } catch {
                /* ignore */
              }
            }}
            className="mt-6 px-4 py-2 rounded-lg text-sm font-medium bg-primary-500 text-white hover:bg-primary-600 transition"
            data-testid="cloud-login-close"
          >
            {t("CloudLogin.close")}
          </button>
        )}
      </div>
    </div>
  );
}
