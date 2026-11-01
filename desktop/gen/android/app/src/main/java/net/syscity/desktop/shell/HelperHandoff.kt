package net.syscity.desktop.shell

import java.security.MessageDigest
import java.security.SecureRandom

/**
 * What the helper and the app agree on when the helper hands over its Binder.
 *
 * The helper runs as the shell user. The app is an untagged app. SELinux
 * allows the *helper* to call into a ContentProvider the app exports, and
 * forbids the app from opening a unix socket in the shell domain — so the
 * direction is decided by the platform, and this file is the protocol that
 * direction needs.
 *
 * The helper is also untrusted in the sense that matters here: anything running
 * as shell on the device could claim to be it. Two things stop that from being
 * a way in — the provider demands `INTERACT_ACROSS_USERS_FULL` (shell has it,
 * ordinary apps do not), and the handoff carries a single-use nonce that only
 * the app knew, written to the helper's stdin through the adb stream that
 * started it. A nonce in the command line would not do: any other shell process
 * can read `/proc/<pid>/cmdline`.
 */
object HelperHandoff {
    /** The only caller this protocol accepts. */
    const val SHELL_UID = 2000

    /** The provider method that carries the handoff. */
    const val METHOD_ATTACH = "attach"

    /** The helper's main class, as launched by `app_process`. */
    const val HELPER_CLASS = "net.syscity.desktop.shell.HelperMain"

    // helper → app
    const val KEY_NONCE = "nonce"
    const val KEY_BINDER = "binder"
    const val KEY_UID = "uid"
    const val KEY_PID = "pid"
    const val KEY_APK = "apk"
    const val KEY_CERT_PIN = "certPin"

    // app → helper
    const val KEY_ACCEPTED = "accepted"
    const val KEY_NEXT = "next"
    const val KEY_TOKEN = "token"
    const val KEY_REASON = "reason"

    /** The provider's authority: `<applicationId>.shell`. */
    fun authority(packageName: String) = "$packageName.shell"

    /** The helper's process name — `app_process --nice-name`, and how a stale one is recognised. */
    fun processName(packageName: String) = "$packageName:shell"

    /**
     * The command `adbd` runs for `exec:` on the device.
     *
     * Three details are load-bearing:
     *
     * * `setsid` — adbd kills the whole process group when the stream closes,
     *   and this stream closes as soon as the handoff succeeds. A detached
     *   session survives it.
     * * the trailing `read _` — keeps `sh` alive until that close, so `setsid`
     *   has taken effect before `sh` exits and takes the helper with it.
     * * the nonce on **stdin**, read by `sh` and echoed into the helper's stdin
     *   through a pipe. Not an argument: `/proc/<pid>/cmdline` is readable by
     *   every shell process on the device.
     *
     * `certPin` is the app signing certificate's SHA-256 — not a secret, and
     * the helper uses it to refuse a handoff meant for a different install.
     */
    fun command(apkPath: String, packageName: String, appUid: Int, certPin: String): String =
        "read n; (echo \"\$n\" | CLASSPATH=${quote(apkPath)} setsid app_process /system/bin " +
            "--nice-name=${quote(processName(packageName))} $HELPER_CLASS " +
            "${quote(packageName)} $appUid ${quote(certPin)} >/dev/null 2>&1 &); read _"

    /** The line written to the adb stream: the nonce `sh` reads. */
    fun stdinLine(nonce: String): ByteArray = "$nonce\n".toByteArray(Charsets.UTF_8)

    /** A fresh single-use nonce. */
    fun newNonce(): String {
        val bytes = ByteArray(32)
        SecureRandom().nextBytes(bytes)
        return bytes.joinToString("") { "%02x".format(it) }
    }

    /** SHA-256 of the signing certificate(s), sorted and comma-joined; null with no signer. */
    fun certPin(certs: List<ByteArray>?): String? {
        if (certs.isNullOrEmpty()) return null
        return certs
            .map { cert ->
                MessageDigest.getInstance("SHA-256")
                    .digest(cert)
                    .joinToString("") { "%02x".format(it) }
            }
            .sorted()
            .joinToString(",")
    }

    private fun quote(s: String) = "'" + s.replace("'", "'\\''") + "'"

    /** Why a handoff was refused. Named, so a log line says which check failed. */
    enum class Reject {
        /** The caller is not the shell user. */
        NotShell,

        /** The uid/pid the caller reported do not match the caller. */
        IdentityMismatch,

        /** The helper is running out of a different APK than this app. */
        WrongApk,

        /** The signing certificate does not match. */
        WrongSigner,

        /** No Binder, or it belongs to some other process. */
        BinderElsewhere,

        /** The nonce is stale or was never issued. */
        BadNonce,
    }

    /**
     * The checks that do not need to touch the Binder.
     *
     * Split out so they can be tested without one: this is the part that
     * decides whether a caller is the helper, and it has to be right.
     */
    fun check(
        callingUid: Int,
        callingPid: Int,
        reportedUid: Int,
        reportedPid: Int,
        helperApk: String?,
        appApk: String,
        helperCertPin: String?,
        appCertPin: String?,
    ): Reject? = when {
        callingUid != SHELL_UID -> Reject.NotShell
        reportedUid != callingUid || reportedPid != callingPid -> Reject.IdentityMismatch
        // Path equality, not package: a second APK the user installed under the
        // same package name would still be a different file.
        helperApk != appApk -> Reject.WrongApk
        // A repackaged APK signed by someone else can keep every other property.
        appCertPin != null && helperCertPin != appCertPin -> Reject.WrongSigner
        else -> null
    }
}
