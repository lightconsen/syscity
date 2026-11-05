package net.syscity.desktop

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.webkit.CookieManager
import android.webkit.JsPromptResult
import android.webkit.JsResult
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import app.tauri.plugin.JSObject
import org.json.JSONObject
import org.json.JSONTokener

/**
 * The phone's own browser, driven by the DOM rather than by pixels.
 *
 * The accessibility tree of a WebView describes the *native* view: "WebView",
 * a scroll bar, and nothing about the page. That is why a login form drawn by
 * JavaScript is opaque to `android_ui_tree`, `android_query` and `android_ocr`
 * — the text is in the page, not in a node. This bridge puts a page in a
 * WebView this process owns and answers questions about it directly.
 *
 * # Why a second WebView and not CDP
 *
 * The desktop CDP stack (`src/tools/browser/`, `chromiumoxide`) is behind the
 * `browser` feature, which the `mobile` profile deliberately leaves off; with
 * it on, the phone build grows by twenty crates for a client that can only talk
 * to a *debuggable* WebView (`setWebContentsDebuggingEnabled` + a
 * `localabstract:webview_devtools_remote_<pid>` forward). Release APKs — the
 * ones that run unattended — have no CDP at all. Evaluating a small script in
 * the page is the path that works in both, and it is the fallback the plan
 * names for exactly this reason.
 *
 * # The wire contract with `src/computer/platform/mobile/web/mod.rs`
 *
 * ```
 * in:  { "op": "open", "url": "https://…", "shim": "<page.js source>" }
 *      { "op": "read" }
 *      { "op": "click", "index": 3 }
 *      { "op": "type", "index": 4, "value": "alice" }
 *      { "op": "script", "script": "document.title" }
 *      { "op": "back" | "forward" | "reload" | "close" | "status" }
 * out: { "ok": true, "page": { "url", "title", "text", "elements": […] } }
 *   or { "ok": false, "code": "NO_WEBVIEW" | "ELEMENT_GONE" | "LOAD_FAILED"
 *                          | "SCRIPT_FAILED" | "BUSY" | "BAD_OP", "error": "…" }
 * ```
 *
 * The shim arrives from Rust once, with `open`, and is re-injected after every
 * page load — a navigation replaces the document, and with it every function
 * the previous page had. Keeping the source in the Rust binary is what stops
 * the two halves from drifting: there is exactly one copy of it.
 *
 * Element indices belong to one `read`. A page that navigated since has new
 * elements at the same numbers, so `click` on a stale index presses something
 * else — the Rust side re-reads after every navigation and the index is only
 * as old as the read that produced it.
 */
class WebViewBridge(private val context: Context) {

  /** Every callback below lands on the main thread, where `handle` is called. */
  private val handler = Handler(Looper.getMainLooper())

  /** The page's own View, created on first use. */
  private var view: WebView? = null

  /** `page.js`, as Rust sent it. Absent means no `open` has happened yet. */
  private var shim: String? = null

  /** Waiting for `onPageFinished`, when an `open`/`back`/`forward`/`reload` is in flight. */
  private var loadReply: ((JSObject) -> Unit)? = null

  /** Bounds the wait for a page that never finishes. */
  private var loadTimer: Runnable? = null

  /** How long a navigation may take before the caller is told it did not finish. */
  private val loadTimeoutMs = 15_000L

  /** One WebView, one command: a second caller is told so instead of interleaving. */
  private var busy = false

  /**
   * Run one operation and answer it.
   *
   * Called on the main thread; every callback below lands there too, so the
   * state fields need no locking. `reply` may be called later than this
   * function returns — `open` waits for the page — and exactly once.
   */
  fun handle(op: String, args: JSONObject, reply: (JSObject) -> Unit) {
    when (op) {
      "open" -> {
        val url = args.optString("url")
        if (url.isBlank()) return reply(fail("BAD_OP", "open needs a `url`"))
        shim = args.optString("shim").ifEmpty { null }
        navigate(url, reply)
      }
      "read" -> evaluate(reply, "JSON.stringify(window.__syscity.snapshot())") { page ->
        JSObject().put("ok", true).put("page", page)
      }
      "click" -> evaluate(
        reply,
        "JSON.stringify(window.__syscity.click(${args.optInt("index", 0)}))",
      ) { answer -> clickAnswer(answer) }
      "type" -> evaluate(
        reply,
        "JSON.stringify(window.__syscity.type(${args.optInt("index", 0)}, " +
          "${JSONObject.quote(args.optString("value"))}))",
      ) { answer -> clickAnswer(answer) }
      "script" -> evaluate(
        reply,
        "JSON.stringify((function(){try{return {ok:true,result:eval(${JSONObject.quote(
          args.optString("script"),
        )})}}catch(e){return {ok:false,code:'SCRIPT_FAILED',error:String(e&&e.message||e)}}})())",
      ) { answer -> objectAnswer(answer) }
      "back" -> history(reply, "back")
      "forward" -> history(reply, "forward")
      "reload" -> history(reply, "reload")
      "close" -> {
        view?.apply { stopLoading(); destroy() }
        view = null
        shim = null
        reply(JSObject().put("ok", true))
      }
      "status" -> {
        val page = view
        reply(
          JSObject()
            .put("ok", true)
            .put("open", page != null)
            .put("url", page?.url ?: "")
            .put("busy", busy),
        )
      }
      else -> reply(fail("BAD_OP", "unknown op $op"))
    }
  }

  // ── The page's lifecycle ─────────────────────────────────────────────────

  /**
   * Give this bridge a page and point it at `url`.
   *
   * The answer is the snapshot of what arrived, not "the load was dispatched":
   * a caller that gets `ok` can act on the elements it carries, and one that
   * gets `LOAD_FAILED` knows the page is not there — as opposed to guessing
   * from a blank read.
   */
  private fun navigate(url: String, reply: (JSObject) -> Unit) {
    val page = ensure() ?: return reply(fail("NO_WEBVIEW", "no WebView is available on this device"))
    if (busy || loadReply != null) return reply(fail("BUSY", "another page operation is still running"))
    waitForLoad(reply)
    page.loadUrl(url)
  }

  /** Answer the pending navigation, once. */
  private fun finishLoad(answer: JSObject) {
    loadTimer?.let(handler::removeCallbacks)
    loadTimer = null
    val reply = loadReply ?: return
    loadReply = null
    busy = false
    reply(answer)
  }

  /** The page, created on first use, or null when the device has no WebView. */
  private fun ensure(): WebView? {
    view?.let { return it }
    return try {
      val created = WebView(context.applicationContext)
      created.settings.apply {
        javaScriptEnabled = true
        domStorageEnabled = true
        databaseEnabled = true
        // A login page is often a desktop layout squeezed into a phone: the
        // wide viewport is what makes its fields exist at all on a narrow
        // screen, and it is what a person's browser would send anyway.
        useWideViewPort = true
        loadWithOverviewMode = true
        setSupportZoom(true)
        builtInZoomControls = false
        // The page's own requests are its own; this bridge never types into
        // them, and blocking mixed content breaks more logins than it saves.
        mixedContentMode = WebSettings.MIXED_CONTENT_COMPATIBILITY_MODE
      }
      CookieManager.getInstance().apply {
        setAcceptCookie(true)
        // SSO redirects are third-party by definition: a login that bounces
        // through an identity provider loses its session without this.
        setAcceptThirdPartyCookies(created, true)
      }
      created.webChromeClient = object : WebChromeClient() {
        // A page that blocks on a dialog would hang the wait; the agent cannot
        // answer one, and dismissing is the only safe automatic answer. A
        // `confirm` is cancelled rather than accepted: "are you sure you want
        // to send?" must not be answered by a bridge. Returning true says the
        // dialog was handled, so the page does not sit on it.
        override fun onJsAlert(
          view: WebView?,
          url: String?,
          message: String?,
          result: JsResult?,
        ): Boolean {
          result?.cancel()
          return true
        }

        override fun onJsConfirm(
          view: WebView?,
          url: String?,
          message: String?,
          result: JsResult?,
        ): Boolean {
          result?.cancel()
          return true
        }

        override fun onJsPrompt(
          view: WebView?,
          url: String?,
          message: String?,
          defaultValue: String?,
          result: JsPromptResult?,
        ): Boolean {
          // Cancelled, which submits null: a prompt is a text field, and the
          // only automatic answer to one is "nothing".
          result?.cancel()
          return true
        }
      }
      created.webViewClient = object : WebViewClient() {
        override fun onPageFinished(view: WebView?, url: String?) {
          // The shim goes in first, then the snapshot is asked for in its
          // callback: `evaluateJavascript` runs its script as soon as it
          // reaches the front of the queue, and asking for a snapshot in the
          // same callback as the injection races it.
          val source = shim
          val pending = loadReply != null
          if (source != null) {
            view?.evaluateJavascript(source) {
              if (pending) snapshotIn(view)
            }
          } else if (pending) {
            snapshotIn(view)
          }
        }

        override fun onReceivedError(
          view: WebView?,
          request: WebResourceRequest?,
          error: WebResourceError?,
        ) {
          // Only the top-level document: a failed image must not fail the page.
          if (request?.isForMainFrame != true) return
          finishLoad(
            fail(
              "LOAD_FAILED",
              "the page could not be loaded (${error?.description ?: "unknown error"})",
            ),
          )
        }
      }
      view = created
      created
    } catch (t: Throwable) {
      android.util.Log.w("SyscityBridge", "WebView is unavailable", t)
      null
    }
  }

  // ── Asking the page a question ───────────────────────────────────────────

  /** Snapshot the document that just finished loading, and answer the wait. */
  private fun snapshotIn(page: WebView?) {
    if (loadReply == null) return
    page?.evaluateJavascript("JSON.stringify(window.__syscity.snapshot())") { raw ->
      val parsed = decode(raw)
      val answer = if (parsed is JSONObject) {
        JSObject().put("ok", true).put("page", parsed)
      } else {
        fail("LOAD_FAILED", "the page loaded but could not be read")
      }
      finishLoad(answer)
    }
  }

  /** Evaluate `expression` (which returns JSON text) and hand the parsed answer on. */
  private fun evaluate(
    reply: (JSObject) -> Unit,
    expression: String,
    build: (Any?) -> JSObject,
  ) {
    val page = ensure() ?: return reply(fail("NO_WEBVIEW", "no WebView is available on this device"))
    if (shim == null) return reply(fail("NO_WEBVIEW", "no page is open; call action 'open' first"))
    // The shim is prepended rather than assumed: a page may have navigated by
    // itself (a meta refresh, a `window.location` in a script) since the last
    // read, and the document it shows now never saw it.
    val source = (shim ?: "") + "\n" + expression
    page.evaluateJavascript(source) { raw ->
      reply(build(decode(raw)))
    }
  }

  /** A `click`/`type` answer: `{ok, name|value}` or `{ok:false, reason:"gone"}`. */
  private fun clickAnswer(answer: Any?): JSObject {
    if (answer !is JSONObject) return fail("ELEMENT_GONE", "the page did not answer")
    if (answer.optBoolean("ok", false)) {
      val out = JSObject().put("ok", true)
      answer.optString("name").takeIf { it.isNotEmpty() }?.let { out.put("name", it) }
      answer.optString("value").takeIf { it.isNotEmpty() }?.let { out.put("value", it) }
      return out
    }
    // The list is re-read before every action, so "gone" is a real answer: the
    // element the caller numbered is not in this list.
    return fail(
      "ELEMENT_GONE",
      "element ${answer.optString("reason")} — read the page again; the numbers belong to one read",
    )
  }

  /** A `script` answer: its `result` (any JSON) or the thrown error. */
  private fun objectAnswer(answer: Any?): JSObject {
    if (answer !is JSONObject) {
      return fail("SCRIPT_FAILED", "the script returned nothing JSON-serializable")
    }
    if (!answer.optBoolean("ok", false)) {
      return fail("SCRIPT_FAILED", answer.optString("error", "the script threw"))
    }
    return JSObject().put("ok", true).put("result", answer.opt("result"))
  }

  /**
   * `back`/`forward`/`reload` — a navigation, so the answer is the page that
   * arrived rather than "the call was made".
   *
   * A history with nowhere to go is a refusal, not a page: `goBack()` with an
   * empty back list leaves the current page up, and answering `ok` would tell
   * the caller it moved.
   */
  private fun history(reply: (JSObject) -> Unit, action: String) {
    val page = view ?: return reply(fail("NO_WEBVIEW", "no page is open"))
    if (busy || loadReply != null) {
      return reply(fail("BUSY", "another page operation is still running"))
    }
    when (action) {
      "back" -> if (!page.canGoBack()) {
        return reply(fail("NO_HISTORY", "there is no earlier page in this session"))
      } else {
        waitForLoad(reply)
        page.goBack()
      }
      "forward" -> if (!page.canGoForward()) {
        return reply(fail("NO_HISTORY", "there is no later page in this session"))
      } else {
        waitForLoad(reply)
        page.goForward()
      }
      else -> {
        waitForLoad(reply)
        page.reload()
      }
    }
  }

  /** Arm the wait that `onPageFinished` answers. */
  private fun waitForLoad(reply: (JSObject) -> Unit) {
    busy = true
    loadReply = reply
    loadTimer = Runnable {
      finishLoad(
        fail(
          "LOAD_FAILED",
          "the page did not finish loading within ${loadTimeoutMs / 1000}s",
        ),
      )
    }.also { handler.postDelayed(it, loadTimeoutMs) }
  }

  private fun fail(code: String, message: String) =
    JSObject().put("ok", false).put("code", code).put("error", message)

  /**
   * `evaluateJavascript` hands back JSON text: a string result is a quoted JSON
   * string, an object is its JSON. The shim stringifies its answers, so the
   * parsed value is usually a String that is itself JSON — unwrap that one
   * level, and nothing else.
   */
  private fun decode(raw: String?): Any? {
    if (raw == null || raw == "null" || raw.isEmpty()) return null
    val outer = runCatching { JSONTokener(raw).nextValue() }.getOrNull() ?: return raw
    if (outer is String) {
      return runCatching { JSONTokener(outer).nextValue() }.getOrDefault(outer)
    }
    return outer
  }
}
