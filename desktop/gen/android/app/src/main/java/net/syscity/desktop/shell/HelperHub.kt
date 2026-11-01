package net.syscity.desktop.shell

import android.content.ContentProvider
import android.content.ContentValues
import android.content.Context
import android.database.Cursor
import android.net.Uri
import android.os.Binder
import android.os.Bundle
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import java.util.concurrent.atomic.AtomicReference

/**
 * Where the helper's Binder lands inside the app.
 *
 * The helper calls in once, after being started, and everything after that is a
 * direct Binder call — no socket, no polling, no reconnection logic. The three
 * things this class owns:
 *
 * * the current [ISyscityShell], with a death recipient so a killed helper is
 *   noticed rather than called into
 * * the single-use nonces, in memory: the app generates one, writes it to the
 *   helper's stdin, and accepts exactly that value once
 * * the token Binder the helper watches, so the app dying tears the helper down
 *   rather than leaving a display behind
 *
 * It is a process-wide singleton because the provider can be called before
 * `Application.onCreate` finishes; making it lazy keeps both orders working.
 */
class HelperHub(context: Context) {

    private val _shell = MutableStateFlow<ISyscityShell?>(null)

    /** The helper's interface, or null when no helper has handed one over. */
    val shell: StateFlow<ISyscityShell?> = _shell.asStateFlow()

    /** Nonces issued and not yet spent. */
    private val pending = AtomicReference<Set<String>>(emptySet())

    /** Watched by the helper: when this dies, so has the app. */
    private val token = Binder()

    /** The helper's own token Binder, once it has handed one over. */
    private val helperToken = AtomicReference<Binder?>(null)

    /** Issue a nonce for an imminent launch. */
    fun issueNonce(): String {
        val nonce = HelperHandoff.newNonce()
        pending.updateAndGet { it + nonce }
        return nonce
    }

    /** Forget every outstanding nonce, e.g. when the user re-pairs. */
    fun forget() {
        pending.set(emptySet())
        _shell.value = null
        helperToken.set(null)
    }

    /** Whether a helper is currently attached. */
    fun isAttached(): Boolean = _shell.value != null

    /**
     * Receive a handoff.
     *
     * Every check is here rather than in the provider so that a test can call
     * this with any combination of arguments.
     */
    @Synchronized
    fun offer(extras: Bundle?, callingUid: Int, callingPid: Int): Bundle {
        val binder = extras?.getBinder(HelperHandoff.KEY_BINDER)
        val nonce = extras?.getString(HelperHandoff.KEY_NONCE)

        val rejected = HelperHandoff.check(
            callingUid = callingUid,
            callingPid = callingPid,
            reportedUid = extras?.getInt(HelperHandoff.KEY_UID, -1) ?: -1,
            reportedPid = extras?.getInt(HelperHandoff.KEY_PID, -1) ?: -1,
            helperApk = extras?.getString(HelperHandoff.KEY_APK),
            appApk = appApk(),
            helperCertPin = extras?.getString(HelperHandoff.KEY_CERT_PIN),
            appCertPin = appCertPin(),
        )
        if (rejected != null) {
            // Name both sides: the paths differ on every reinstall, and "Wrong"
            // with no values is the least useful thing a check can say.
            return reject(
                rejected,
                "helper=${extras?.getString(HelperHandoff.KEY_APK)} app=${appApk()}",
            )
        }

        // Spend the nonce before touching the Binder: a wrong nonce must not be
        // a way to get this process to asInterface an attacker's object.
        if (nonce == null || !spendNonce(nonce)) return reject(HelperHandoff.Reject.BadNonce)

        val shell = ISyscityShell.Stub.asInterface(binder)
        // The Binder's own answer to "who are you", which a caller cannot fake:
        // `identity()` runs in the process that owns the Binder.
        val identity = runCatching { shell.identity() }.getOrNull()
        if (identity == null || identity.size < 2 ||
            identity[0] != callingUid || identity[1] != callingPid
        ) {
            return reject(HelperHandoff.Reject.BinderElsewhere)
        }

        runCatching {
            binder!!.linkToDeath({ _shell.compareAndSet(shell, null) }, 0)
        }.onFailure {
            return reject(HelperHandoff.Reject.BinderElsewhere)
        }

        _shell.value = shell
        Log.i(TAG, "helper attached: pid=$callingPid uid=$callingUid")
        return Bundle().apply {
            putBoolean(HelperHandoff.KEY_ACCEPTED, true)
            putString(HelperHandoff.KEY_NEXT, issueNonce())
            putBinder(HelperHandoff.KEY_TOKEN, token)
        }
    }

    /** The helper's token Binder, for the app to link to death. */
    fun takeHelperToken(token: Binder) {
        helperToken.set(token)
    }

    private fun spendNonce(nonce: String): Boolean {
        val current = pending.get()
        if (nonce !in current) return false
        // `compareAndSet` so two concurrent handoffs cannot both spend it.
        return pending.compareAndSet(current, current - nonce)
    }

    private fun reject(why: HelperHandoff.Reject, detail: String? = null): Bundle {
        Log.w(TAG, "helper handoff rejected: $why${detail?.let { " ($it)" } ?: ""}")
        return Bundle().apply {
            putBoolean(HelperHandoff.KEY_ACCEPTED, false)
            putString(HelperHandoff.KEY_REASON, why.name)
        }
    }

    private fun appApk(): String = app.applicationInfo.sourceDir

    private fun appCertPin(): String? = runCatching {
        val info = app.packageManager.getPackageInfo(
            app.packageName,
            android.content.pm.PackageManager.GET_SIGNING_CERTIFICATES,
        )
        val signers = info.signingInfo?.apkContentsSigners ?: return@runCatching null
        HelperHandoff.certPin(signers.map { it.toByteArray() })
    }.getOrNull()

    private val app: Context = context.applicationContext

    private companion object {
        const val TAG = "SyscityShellHub"
    }
}

/**
 * The app's side of the handoff.
 *
 * `exported="true"` is required because the caller is a different uid, and it
 * is the manifest's `android:permission` that keeps ordinary apps out:
 * `INTERACT_ACROSS_USERS_FULL` is held by shell and system, not by installed
 * apps. [HelperHub.offer] is what decides whether the shell that called is
 * *our* shell.
 */
class ShellHandoffProvider : ContentProvider() {

    override fun onCreate() = true

    override fun call(method: String, arg: String?, extras: Bundle?): Bundle? {
        if (method != HelperHandoff.METHOD_ATTACH) return null
        val ctx = context ?: return null
        return ShellServices.hub(ctx).offer(extras, Binder.getCallingUid(), Binder.getCallingPid())
    }

    override fun query(
        uri: Uri,
        projection: Array<out String>?,
        selection: String?,
        selectionArgs: Array<out String>?,
        sortOrder: String?,
    ): Cursor? = null

    override fun getType(uri: Uri): String? = null
    override fun insert(uri: Uri, values: ContentValues?): Uri? = null
    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?) = 0
    override fun update(
        uri: Uri,
        values: ContentValues?,
        selection: String?,
        selectionArgs: Array<out String>?,
    ) = 0
}

/**
 * Process-wide holder for the hub.
 *
 * The provider can be called before `Application.onCreate` has run — the
 * system may start the provider first — so both entry points have to be able to
 * construct this, and only one of them gets to. A synchronised lazy is the
 * simplest thing that is true in both orders.
 */
object ShellServices {
    @Volatile
    private var hubInstance: HelperHub? = null

    fun hub(context: Context): HelperHub =
        hubInstance ?: synchronized(this) {
            hubInstance ?: HelperHub(context).also { hubInstance = it }
        }

    /** For the state that only the app process can provide (the bridge command). */
    fun peek(): HelperHub? = hubInstance

    /** Read an `ISyscityShell` for a display call, or null when no helper is attached. */
    fun shell(): ISyscityShell? = hubInstance?.shell?.value

    /** The path a display's latest frame was written to, for the caller to open. */
    fun frame(displayId: Int): String? =
        shell()?.let { runCatching { it.frame(displayId) }.getOrNull() }
}
