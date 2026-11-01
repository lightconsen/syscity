package net.syscity.desktop

import android.os.Build
import android.os.Bundle
import android.system.Os
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.webkit.WebSettingsCompat
import androidx.webkit.WebViewFeature
import java.io.File

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    // Point syscity's data root at the app sandbox *before* the Rust runtime
    // initializes: `super.onCreate` loads the native lib and calls the entry
    // point, and `dirs.rs` reads SYSCITY_HOME at first use.
    Os.setenv("SYSCITY_HOME", File(filesDir, "syscity").absolutePath, true)
    // Point the AndroidShellRunner at the APK's extracted native libraries
    // (`jniLibs`) so bundled binaries (sh/toybox) can be exec'd from there.
    Os.setenv("SYSCITY_NATIVE_LIB_DIR", applicationInfo.nativeLibraryDir, true)
    // Debug builds only: let `debug.tool.invoke` run a platform tool without an
    // agent turn, so on-device behaviour (the virtual display, the accessibility
    // tree) can be checked without spending a model call. Release builds never
    // set this, and the method refuses while it is absent — it runs tools
    // outside the approval flow.
    if (BuildConfig.DEBUG) {
      Os.setenv("SYSCITY_DEBUG_TOOL_INVOKE", "1", true)
    }
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Keep the process alive for long agent turns when backgrounded (§4.7).
    AgentRuntimeService.start(this)
  }

  // Called by Tauri after the WebView is created. Follow the system dark mode
  // so `prefers-color-scheme: dark` reaches the SPA (parity with iOS
  // WKWebView). API 33+ uses the native algorithmic darkening; older APIs use
  // the androidx fallback.
  override fun onWebViewCreate(webView: WebView) {
    if (Build.VERSION.SDK_INT >= 33) {
      webView.settings.isAlgorithmicDarkeningAllowed = true
    } else if (WebViewFeature.isFeatureSupported(WebViewFeature.FORCE_DARK)) {
      WebSettingsCompat.setForceDark(webView.settings, WebSettingsCompat.FORCE_DARK_AUTO)
    }
    super.onWebViewCreate(webView)
  }

  override fun onDestroy() {
    AgentRuntimeService.stop(this)
    super.onDestroy()
  }
}
