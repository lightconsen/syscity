package net.syscity.desktop

import android.Manifest
import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ActivityNotFoundException
import android.content.Intent
import android.graphics.BitmapFactory
import android.location.Location
import android.location.LocationListener
import android.location.LocationManager
import android.net.Uri
import android.provider.MediaStore
import android.provider.Settings
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.provider.OpenableColumns
import androidx.activity.result.ActivityResult
import androidx.core.app.NotificationCompat
import androidx.core.content.FileProvider
import app.tauri.PermissionState
import net.syscity.desktop.shell.ShellServices
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.Executors

/**
 * Native device capability bridge (mobile-migration §4.1/§4.2/§4.3).
 * Loopback-ADB pairing (§4.5) is intentionally NOT here: the bundled adb
 * client runs Rust-side via the platform process_runner, so the plugin never
 * execs adb.
 *
 * Registered from Rust via `register_android_plugin("net.syscity.desktop",
 * "DevicePlugin")` in `desktop/src/lib.rs`. The Rust runtime reaches these
 * methods through the `DeviceBridge` trait; each `@Command` method name must
 * match the `CMD_*` constants in `src/device/mod.rs` verbatim, because Tauri
 * dispatches the command string by method name with no transformation.
 *
 * All `@Command` bodies run on the Android main thread. Work that can block
 * (SAF stream copy) or that needs a Looper (location fixes) is dispatched
 * accordingly, and `invoke.resolve` is always called back on the main thread.
 */
@TauriPlugin(
  permissions = [
    Permission(strings = [Manifest.permission.CAMERA], alias = "camera"),
    Permission(
      strings = [
        Manifest.permission.ACCESS_FINE_LOCATION,
        Manifest.permission.ACCESS_COARSE_LOCATION,
      ],
      alias = "location",
    ),
    Permission(strings = [Manifest.permission.POST_NOTIFICATIONS], alias = "notifications"),
  ],
)
class DevicePlugin(activity: Activity) : Plugin(activity) {

  /** The base `Plugin` keeps its activity private; keep our own reference. */
  private val appActivity = activity

  /** Serial executor for file copies; the plugin lives for the app lifetime. */
  private val executor = Executors.newSingleThreadExecutor()

  private val mainHandler = Handler(Looper.getMainLooper())

  /** In-flight camera capture target, resolved by the activity-result callback. */
  private var pendingCameraFile: File? = null

  companion object {
    private const val CHANNEL_NOTIFY = "agent_notify"
  }

  // ─────────────────────────────────────────────
  // Permission status / request
  // ─────────────────────────────────────────────

  /** `permissionStatus` — report the grant state of a runtime permission. */
  @Command
  fun permissionStatus(invoke: Invoke) {
    val alias = invoke.getArgs().getString("permission")
    val state = getPermissionState(alias)
    if (state == null) {
      invoke.reject("Unknown permission alias '$alias'", "UNKNOWN_PERMISSION")
      return
    }
    resolvePermissionState(invoke, state)
  }

  /** `requestPermission` — ask the user to grant a runtime permission. */
  @Command
  fun requestPermission(invoke: Invoke) {
    val alias = invoke.getArgs().getString("permission")
    val state = getPermissionState(alias)
    if (state == null) {
      invoke.reject("Unknown permission alias '$alias'", "UNKNOWN_PERMISSION")
      return
    }
    if (state == PermissionState.GRANTED) {
      resolvePermissionState(invoke, state)
      return
    }
    requestPermissionForAliases(arrayOf(alias), invoke, "onPermissionResult")
  }

  @PermissionCallback
  fun onPermissionResult(invoke: Invoke) {
    val alias = invoke.getArgs().getString("permission")
    val state = getPermissionState(alias) ?: PermissionState.DENIED
    resolvePermissionState(invoke, state)
  }

  private fun resolvePermissionState(invoke: Invoke, state: PermissionState) {
    val out = JSObject()
    out.put("granted", state == PermissionState.GRANTED)
    out.put("state", state.toString())
    invoke.resolve(out)
  }

  // ─────────────────────────────────────────────
  // Camera (§4.1)
  // ─────────────────────────────────────────────

  /** `captureCamera` — open the camera app to take a photo. */
  @Command
  fun captureCamera(invoke: Invoke) {
    if (getPermissionState("camera") != PermissionState.GRANTED) {
      invoke.reject("Camera permission not granted", "PERMISSION_DENIED")
      return
    }
    val cameraDir = File(appActivity.filesDir, "syscity/camera")
    if (!cameraDir.exists()) cameraDir.mkdirs()
    val name = "IMG_" + SimpleDateFormat("yyyyMMdd_HHmmss", Locale.US).format(Date()) + ".jpg"
    val file = File(cameraDir, name)
    val uri = FileProvider.getUriForFile(
      appActivity,
      "${appActivity.packageName}.fileprovider",
      file,
    )
    val intent = Intent(MediaStore.ACTION_IMAGE_CAPTURE)
      .putExtra(MediaStore.EXTRA_OUTPUT, uri)
    if (intent.resolveActivity(appActivity.packageManager) == null) {
      invoke.reject("No camera app available", "NO_CAMERA_APP")
      return
    }
    pendingCameraFile = file
    startActivityForResult(invoke, intent, "onCameraResult")
  }

  @ActivityCallback
  fun onCameraResult(invoke: Invoke, result: ActivityResult) {
    val file = pendingCameraFile
    pendingCameraFile = null
    if (result.resultCode != Activity.RESULT_OK || file == null) {
      invoke.reject("Camera capture cancelled", "CANCELLED")
      return
    }
    if (!file.exists() || file.length() == 0L) {
      invoke.reject("Camera produced no image", "CAPTURE_FAILED")
      return
    }
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeFile(file.absolutePath, bounds)
    val out = JSObject()
    out.put("path", "camera/" + file.name)
    out.put("width", bounds.outWidth)
    out.put("height", bounds.outHeight)
    invoke.resolve(out)
  }

  // ─────────────────────────────────────────────
  // Location (§4.1)
  // ─────────────────────────────────────────────

  /** `getLocation` — return the best location fix, waiting up to 10 s. */
  @Command
  fun getLocation(invoke: Invoke) {
    if (getPermissionState("location") != PermissionState.GRANTED) {
      invoke.reject("Location permission not granted", "PERMISSION_DENIED")
      return
    }
    val lm = appActivity.getSystemService(LocationManager::class.java)
    val providers = listOf(LocationManager.GPS_PROVIDER, LocationManager.NETWORK_PROVIDER)
      .filter { runCatching { lm.isProviderEnabled(it) }.getOrDefault(false) }
    if (providers.isEmpty()) {
      invoke.reject("No location providers enabled", "NO_LOCATION_PROVIDER")
      return
    }
    val fixes = java.util.Collections.synchronizedList(mutableListOf<Location>())
    val listener = object : LocationListener {
      override fun onLocationChanged(location: Location) {
        fixes.add(location)
      }
    }
    providers.forEach { lm.requestLocationUpdates(it, 0L, 0f, listener, Looper.getMainLooper()) }
    val timeout = Runnable {
      providers.forEach { lm.removeUpdates(listener) }
      val best = fixes.maxByOrNull { it.accuracy } ?: runCatching {
        lm.getLastKnownLocation(providers.first())
      }.getOrNull()
      if (best != null) {
        resolveLocation(invoke, best)
      } else {
        invoke.reject("Timed out waiting for a location fix", "LOCATION_TIMEOUT")
      }
    }
    mainHandler.postDelayed(timeout, 10_000L)
  }

  private fun resolveLocation(invoke: Invoke, loc: Location) {
    val out = JSObject()
    out.put("latitude", loc.latitude)
    out.put("longitude", loc.longitude)
    out.put("accuracy_meters", loc.accuracy.toDouble())
    out.put("timestamp_ms", loc.time)
    invoke.resolve(out)
  }

  // ─────────────────────────────────────────────
  // Notification / haptic (§4.1)
  // ─────────────────────────────────────────────

  /** `showNotification` — post a heads-up notification. */
  @Command
  fun showNotification(invoke: Invoke) {
    val args = invoke.getArgs()
    val title = args.getString("title", "Syscity")
    val body = args.getString("body", "")
    val manager = appActivity.getSystemService(NotificationManager::class.java)
    val channel = NotificationChannel(
      CHANNEL_NOTIFY,
      "Agent notifications",
      NotificationManager.IMPORTANCE_DEFAULT,
    )
    manager.createNotificationChannel(channel)
    val openApp = PendingIntent.getActivity(
      appActivity,
      0,
      Intent(appActivity, MainActivity::class.java),
      PendingIntent.FLAG_IMMUTABLE,
    )
    val notification = NotificationCompat.Builder(appActivity, CHANNEL_NOTIFY)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(title)
      .setContentText(body)
      .setContentIntent(openApp)
      .setAutoCancel(true)
      .build()
    val id = (System.currentTimeMillis() and 0x7fffffff).toInt()
    manager.notify(id, notification)
    val out = JSObject()
    out.put("delivered", true)
    invoke.resolve(out)
  }

  /** `vibrate` — trigger a short vibration. */
  @Command
  fun vibrate(invoke: Invoke) {
    val durationMs = invoke.getArgs().getInteger("duration_ms", 200)
    val effect = VibrationEffect.createOneShot(
      durationMs.toLong(),
      VibrationEffect.DEFAULT_AMPLITUDE,
    )
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
      val vm = appActivity.getSystemService(VibratorManager::class.java)
      vm.defaultVibrator.vibrate(effect)
    } else {
      @Suppress("DEPRECATION")
      val vibrator = appActivity.getSystemService(Vibrator::class.java)
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
        vibrator.vibrate(effect)
      } else {
        @Suppress("DEPRECATION")
        vibrator.vibrate(durationMs.toLong())
      }
    }
    val out = JSObject()
    out.put("duration_ms", durationMs)
    invoke.resolve(out)
  }

  // ─────────────────────────────────────────────
  // URL handoff (dialer / mail / messages / browser)
  // ─────────────────────────────────────────────

  /** `openUrl` — hand a URL to the OS (`ACTION_VIEW`); the user confirms in
   *  the target app. Rust-side validation restricts schemes to
   *  tel/mailto/sms/https/http; this side re-checks and reports whether any
   *  application accepted the handoff. */
  @Command
  fun openUrl(invoke: Invoke) {
    // `getString(key, default)` is nullable in this SDK; optString is not,
    // and an empty URL is rejected below anyway.
    val url = invoke.getArgs().optString("url", "")
    val scheme = url.substringBefore(':').lowercase(Locale.US)
    if (url.isEmpty() ||
      url.any { it.isISOControl() } ||
      scheme !in setOf("tel", "mailto", "sms", "https", "http")
    ) {
      invoke.reject("Unsupported or malformed URL", code = "INVALID_ARGUMENTS")
      return
    }
    val intent = Intent(Intent.ACTION_VIEW, Uri.parse(url))
    try {
      appActivity.startActivity(intent)
      val out = JSObject()
      out.put("opened", true)
      out.put("url", url)
      invoke.resolve(out)
    } catch (_: ActivityNotFoundException) {
      val out = JSObject()
      out.put("opened", false)
      out.put("url", url)
      invoke.resolve(out)
    }
  }

  // ─────────────────────────────────────────────
  // SAF file picker (§4.2)
  // ─────────────────────────────────────────────

  /** `pickFile` — open the system document picker (SAF). */
  @Command
  fun pickFile(invoke: Invoke) {
    val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
      addCategory(Intent.CATEGORY_OPENABLE)
      type = "*/*"
    }
    if (intent.resolveActivity(appActivity.packageManager) == null) {
      invoke.reject("No document picker available", "NO_PICKER")
      return
    }
    startActivityForResult(invoke, intent, "onPickFileResult")
  }

  @ActivityCallback
  fun onPickFileResult(invoke: Invoke, result: ActivityResult) {
    val uri = result.data?.data
    if (result.resultCode != Activity.RESULT_OK || uri == null) {
      invoke.reject("File pick cancelled", "CANCELLED")
      return
    }
    val name = queryDisplayName(uri) ?: (uri.lastPathSegment ?: "file")
    val safeName = sanitizeName(name)
    executor.execute {
      try {
        val outDir = File(appActivity.filesDir, "syscity/user-files")
        outDir.mkdirs()
        var target = uniqueTarget(outDir, safeName)
        val input = appActivity.contentResolver.openInputStream(uri)
          ?: throw IllegalStateException("Cannot open selected file")
        input.use { ins ->
          target.outputStream().use { outs -> ins.copyTo(outs) }
        }
        val out = JSObject()
        out.put("path", "user-files/" + target.name)
        out.put("name", target.name)
        out.put("size_bytes", target.length())
        mainHandler.post { invoke.resolve(out) }
      } catch (e: Exception) {
        mainHandler.post { invoke.reject(e.message ?: "copy failed", "COPY_FAILED") }
      }
    }
  }

  private fun queryDisplayName(uri: Uri): String? {
    return runCatching {
      val cursor = appActivity.contentResolver.query(
        uri,
        arrayOf(OpenableColumns.DISPLAY_NAME),
        null,
        null,
        null,
      )
      cursor?.use { c ->
        if (c.moveToFirst()) {
          val idx = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
          if (idx >= 0) c.getString(idx) else null
        } else {
          null
        }
      }
    }.getOrNull()
  }

  /** Strip any path separators / traversal before it becomes a filename. */
  private fun sanitizeName(name: String): String {
    var s = name
    if (s.contains('/')) s = s.substringAfterLast('/')
    if (s.contains('\\')) s = s.substringAfterLast('\\')
    s = s.replace("..", "_").trim()
    return s.ifBlank { "file" }
  }

  /** Return a non-colliding File, appending `_1`, `_2`, … on conflicts. */
  private fun uniqueTarget(dir: File, name: String): File {
    var target = File(dir, name)
    var counter = 1
    while (target.exists()) {
      val dot = name.lastIndexOf('.')
      val base = if (dot > 0) name.substring(0, dot) else name
      val ext = if (dot > 0) name.substring(dot) else ""
      target = File(dir, "${base}_$counter$ext")
      counter++
    }
    return target
  }

  // ─────────────────────────────────────────────
  // Cron background wake (§4.3)
  // ─────────────────────────────────────────────

  /** `syncCronSchedule` — re-arm WorkManager alarms from a full schedule snapshot. */
  @Command
  fun syncCronSchedule(invoke: Invoke) {
    val jobs = mutableListOf<CronWakeScheduler.Job>()
    val arr = invoke.getArgs().optJSONArray("jobs")
    if (arr != null) {
      for (i in 0 until arr.length()) {
        val obj = arr.optJSONObject(i) ?: continue
        val id = obj.optString("id")
        if (id.isBlank()) continue
        val atMs = if (obj.isNull("at_ms")) null else obj.optLong("at_ms")
        jobs.add(CronWakeScheduler.Job(id, atMs))
      }
    }
    executor.execute {
      CronWakeScheduler.sync(appActivity.applicationContext, jobs)
      val out = JSObject()
      out.put("scheduled", jobs.size)
      mainHandler.post { invoke.resolve(out) }
    }
  }

  // ─────────────────────────────────────────────
  // Accessibility tree (on-device agent reads the screen)
  // ─────────────────────────────────────────────

  /**
   * `accessibilityTree` — the focused window as JSON.
   *
   * Answers `{"enabled": false}` when the service is off, which the Rust side
   * treats as "fall back to ADB" rather than as a failure: the desktop agent
   * driving a remote phone reaches this same code path with no service at all.
   */
  @Command
  fun accessibilityTree(invoke: Invoke) {
    // `display` selects a virtual display; absent means the user's screen.
    val displayId = if (invoke.getArgs().has("display")) {
      invoke.getArgs().optInt("display", 0)
    } else {
      null
    }
    val snapshot = A11yBridge.snapshot(displayId)
    if (snapshot == null) {
      invoke.resolve(JSObject().put("enabled", false))
      return
    }
    invoke.resolve(snapshot)
  }

  /** `accessibilityStatus` — is the service switched on, and is it connected? */
  @Command
  fun accessibilityStatus(invoke: Invoke) {
    val out = JSObject()
    out.put("enabled", A11yBridge.isEnabled(appActivity))
    out.put("connected", A11yBridge.isConnected())
    invoke.resolve(out)
  }

  /**
   * `openAccessibilitySettings` — take the user where they can enable it.
   *
   * The grant itself is deliberately not automated: an app that can turn on its
   * own screen-reading service without the user opening that screen is not
   * something this should be able to do.
   */
  @Command
  fun openAccessibilitySettings(invoke: Invoke) {
    val out = JSObject()
    try {
      val intent = Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
      appActivity.startActivity(intent)
      out.put("opened", true)
    } catch (e: Exception) {
      invoke.reject("Could not open the accessibility settings: ${e.message}", "NO_SETTINGS")
      return
    }
    invoke.resolve(out)
  }

  // ─────────────────────────────────────────────
  // Virtual display (shell helper, second screen)
  // ─────────────────────────────────────────────

  /**
   * `virtualDisplay` — forward one op to the shell-side display helper.
   *
   * The helper is reached over Binder, not a socket: SELinux forbids an
   * `untrusted_app` from opening a unix socket in the `shell` domain, while a
   * ContentProvider handoff (the helper calls *us*) is the direction the policy
   * allows. See `shell/HelperHub.kt`.
   *
   * A pass-through on purpose: the protocol lives in
   * `src/computer/platform/mobile/display.rs`, which is where it is validated
   * and where the tests are. This side's jobs are getting off the main thread
   * and moving frames — a PNG is over a Binder transaction's size budget, so it
   * comes back as a file descriptor.
   */
  @Command
  fun virtualDisplay(invoke: Invoke) {
    val args = invoke.getArgs()
    val op = args.optString("op")
    val displayId = args.optInt("display", -1)
    executor.execute {
      val shell = ShellServices.shell()
      val out = JSObject()
      try {
        when (op) {
          "nonce" -> {
            // The hub issues it, so the hub is what accepts it: a nonce minted
            // anywhere else is one this side has never heard of, which is
            // exactly how a launch looks when the two disagree (`BadNonce`).
            val hub = ShellServices.hub(appActivity)
            out.put("ok", true).put("nonce", hub.issueNonce())
          }
          "ping" -> {
            out.put("ok", shell != null)
            out.put("attached", shell != null)
          }
          "create" -> {
            if (shell == null) {
              out.put("ok", false).put("code", "NO_HELPER")
              out.put("error", "no display helper is attached")
            } else {
              val id = shell.createDisplay(
                args.optInt("width", 0), args.optInt("height", 0), args.optInt("dpi", 0),
              )
              if (id < 0) {
                out.put("ok", false).put("code", "CREATE_FAILED")
                out.put("error", "the helper could not create a display")
              } else {
                out.put("ok", true).put("display", id)
                  .put("width", args.optInt("width", 1080))
                  .put("height", args.optInt("height", 2400))
              }
            }
          }
          "destroy" -> {
            shell?.releaseDisplay(displayId)
            out.put("ok", true)
          }
          "launch" -> {
            val status = shell?.startApp(displayId, args.optString("package")) ?: -1
            if (status == 0) out.put("ok", true)
            else out.put("ok", false).put("code", "LAUNCH_FAILED")
              .put("error", "the app would not start on display $displayId")
          }
          "tap" -> {
            val ok = shell?.injectTap(displayId, args.optInt("x"), args.optInt("y")) ?: false
            out.put("ok", ok)
          }
          "swipe" -> {
            val ok = shell?.injectSwipe(
              displayId,
              args.optInt("x"), args.optInt("y"), args.optInt("x2"), args.optInt("y2"),
              args.optInt("duration", 300),
            ) ?: false
            out.put("ok", ok)
          }
          "text" -> {
            val ok = shell?.injectText(displayId, args.optString("text")) ?: false
            out.put("ok", ok)
          }
          "key" -> {
            val keycode = args.optString("keycode")
            val code = android.view.KeyEvent::class.java
              .getField(keycode).getInt(null)
            val ok = shell?.injectKey(displayId, code) ?: false
            out.put("ok", ok)
          }
          "screenshot" -> {
            val pfd = ShellServices.frame(displayId)
            if (pfd == null) {
              out.put("ok", false).put("code", "NO_FRAME")
              out.put("error", "the display produced no frame")
            } else {
              // Read the fd here and hand the bytes back as text: the bridge
              // carries JSON, and the frame is already a file the app can open.
              val bytes = android.os.ParcelFileDescriptor.AutoCloseInputStream(pfd)
                .use { it.readBytes() }
              out.put("ok", true)
                .put("png", android.util.Base64.encodeToString(bytes, android.util.Base64.NO_WRAP))
            }
          }
          else -> out.put("ok", false).put("code", "UNKNOWN_OP").put("error", "unknown op $op")
        }
        mainHandler.post { invoke.resolve(out) }
      } catch (e: Exception) {
        mainHandler.post {
          invoke.reject(
            "the display helper call failed: ${e.message}",
            "DISPLAY_UNAVAILABLE",
          )
        }
      }
    }
  }
}
