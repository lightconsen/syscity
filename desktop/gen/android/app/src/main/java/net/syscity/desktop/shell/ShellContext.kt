package net.syscity.desktop.shell

import android.annotation.SuppressLint
import android.app.Application
import android.app.Instrumentation
import android.content.AttributionSource
import android.content.Context
import android.content.ContextWrapper
import android.content.pm.ApplicationInfo
import android.os.Build
import android.util.Log

/**
 * A system Context that says it belongs to the shell user.
 *
 * Two separate problems, both of which surface as "permission denied" with no
 * hint that the *identity* is the problem:
 *
 * **1. System services check that the package a caller names belongs to the
 * caller's uid.** `getSystemContext()` reports `android` (uid 1000), and this
 * process runs as `shell` (2000). Creating a virtual display is refused with
 * `packageName must match the calling uid`. Reporting `com.android.shell` — the
 * package that actually owns uid 2000 — is both true and what the check wants.
 * `getOpPackageName` and `getAttributionSource` are the same fact said three
 * ways, and each is consulted by a different service, so all three are
 * overridden.
 *
 * **2. `app_process` is not registered with ActivityManager as an app.**
 * `contentResolver.call(...)` on another app's ContentProvider fails with
 * `SecurityException: Unable to find app for caller … when getting content
 * provider`: the system looks up the calling pid in its process records, finds
 * nothing, and refuses to start the provider on behalf of an unknown caller.
 * The fix is the one scrcpy uses for the same reason — construct an
 * `ActivityThread`, publish it as the current one, and give it the minimum an
 * app record needs (`mBoundApplication` with a package name, and a
 * `ConfigurationController` from API 31).
 *
 * Adapted from scrcpy (Genymobile, Apache-2.0), which solved this for shell
 * processes long before this project needed it.
 */
internal class ShellContext private constructor(base: Context) : ContextWrapper(base) {

    override fun getPackageName(): String = SHELL_PACKAGE

    override fun getOpPackageName(): String = SHELL_PACKAGE

    override fun getAttributionSource(): AttributionSource =
        AttributionSource.Builder(SHELL_UID).setPackageName(SHELL_PACKAGE).build()

    override fun getApplicationContext(): Context = this

    companion object {
        private const val TAG = "SyscityShell"

        /** The package that owns uid 2000. */
        const val SHELL_PACKAGE = "com.android.shell"

        const val SHELL_UID = 2000

        /**
         * Build the context, registering this process as an app on the way.
         *
         * Every step is attempted and none is fatal: a device where one of the
         * reflective fields has moved still gets a usable Context, and the
         * failure that matters (the provider refusing the call) names itself.
         */
        @SuppressLint("PrivateApi", "DiscouragedPrivateApi")
        fun create(): Context? = runCatching {
            val activityThread = Class.forName("android.app.ActivityThread")
            val thread = activityThread.getMethod("currentActivityThread").invoke(null)
                ?: newActivityThread(activityThread)

            @Suppress("UNCHECKED_CAST")
            val system = activityThread
                .getDeclaredMethod("getSystemContext")
                .invoke(thread) as Context

            val context = ShellContext(system)

            // An app record needs an Application instance; without one some
            // services treat the caller as not-yet-started.
            val initial = activityThread
                .getDeclaredField("mInitialApplication")
                .apply { isAccessible = true }
                .get(thread)
            if (initial == null) {
                runCatching {
                    val application = Instrumentation.newApplication(Application::class.java, context)
                    activityThread.getDeclaredField("mInitialApplication")
                        .apply { isAccessible = true }
                        .set(thread, application)
                }.onFailure { Log.w(TAG, "no initial Application: ${it.message}") }
            }

            context
        }.onFailure { Log.e(TAG, "could not build the shell Context", it) }.getOrNull()

        /**
         * `app_process` without an app record — the state that makes
         * `getSystemContext()` usable and `contentResolver.call` rejected.
         */
        @SuppressLint("PrivateApi", "DiscouragedPrivateApi", "SoonBlockedPrivateApi")
        private fun newActivityThread(activityThread: Class<*>): Any {
            val thread = activityThread.getDeclaredConstructor()
                .apply { isAccessible = true }
                .newInstance()
            activityThread.getDeclaredField("sCurrentActivityThread")
                .apply { isAccessible = true }
                .set(null, thread)
            // The real system process sets this; a thread claiming to be the
            // main one without it takes a different (and wrong) code path.
            activityThread.getDeclaredField("mSystemThread")
                .apply { isAccessible = true }
                .setBoolean(thread, true)

            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                runCatching {
                    val controllerClass = Class.forName("android.app.ConfigurationController")
                    val internal = Class.forName("android.app.ActivityThreadInternal")
                    val controller = controllerClass
                        .getDeclaredConstructor(internal)
                        .apply { isAccessible = true }
                        .newInstance(thread)
                    activityThread.getDeclaredField("mConfigurationController")
                        .apply { isAccessible = true }
                        .set(thread, controller)
                }.onFailure { Log.w(TAG, "no ConfigurationController: ${it.message}") }
            }

            // The app record itself: ActivityManager matches a provider caller
            // against this, so it has to exist and to name a package that owns
            // this uid.
            runCatching {
                val bindDataClass = Class.forName("android.app.ActivityThread\$AppBindData")
                val data = bindDataClass.getDeclaredConstructor()
                    .apply { isAccessible = true }
                    .newInstance()
                bindDataClass.getDeclaredField("appInfo")
                    .apply { isAccessible = true }
                    .set(data, ApplicationInfo().apply { packageName = SHELL_PACKAGE })
                activityThread.getDeclaredField("mBoundApplication")
                    .apply { isAccessible = true }
                    .set(thread, data)
            }.onFailure { Log.w(TAG, "no AppBindData: ${it.message}") }

            return thread
        }
    }
}
