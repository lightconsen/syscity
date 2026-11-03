package net.syscity.desktop.shell

import android.content.ClipData
import android.content.ClipboardManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log

/**
 * The clipboard, swapped in for a paste and put back.
 *
 * `input -d text` types through the device's key character map, so it drops or
 * mangles anything outside ASCII — CJK in particular. The clipboard carries
 * whatever the app can render, and Ctrl+V pastes it.
 *
 * The rule that makes this safe to do to somebody's phone: **only touch the
 * clipboard when the current contents can be read back**. A clipboard that
 * reads as empty may actually be "present but hidden" — the platform silently
 * refuses some reads — and overwriting that would lose whatever the user had.
 * When it cannot be read, nothing is changed and the caller is told.
 *
 * The restore only writes back to a clipboard that still holds our text: if the
 * user copied something of their own in the meantime, putting the old contents
 * back would delete what they just copied.
 */
internal class ShellClipboard {

    private val lock = Any()
    private var saved: ClipData? = null
    private var savedPresent = false
    private var ours: String? = null
    private var held = false

    /**
     * The clipboard service, bound to a Context that reports the shell package.
     *
     * `getSystemService` is not enough: the platform Context this process got
     * from `ActivityThread` reports `android` as its package, and the clipboard
     * service checks that against the caller's uid —
     * `SecurityException: Package android does not belong to 2000`. The hidden
     * `ClipboardManager(Context, Handler)` constructor keeps whatever Context it
     * is given, and `ShellContext` names `com.android.shell`, which owns uid
     * 2000. This is why it is reflected rather than looked up.
     */
    private val manager: ClipboardManager? by lazy {
        runCatching {
            val context = ShellContext.create() ?: return@runCatching null
            val handler = Handler(Looper.getMainLooper() ?: Looper.myLooper() ?: return@runCatching null)
            ClipboardManager::class.java
                .getDeclaredConstructor(android.content.Context::class.java, Handler::class.java)
                .apply { isAccessible = true }
                .newInstance(context, handler)
        }.getOrNull()
    }

    /**
     * Put [text] on the clipboard. Returns false — with nothing changed — when
     * the user's own clipboard could not be read first.
     */
    fun put(text: String): Boolean = synchronized(lock) {
        val manager = manager ?: return false
        if (!held) {
            val prior = runCatching { manager.primaryClip }.getOrNull()
            val present = present(manager)
            when (ShellClipRead.classify(prior != null, present)) {
                Read.UNREADABLE -> {
                    Log.i(TAG, "clipboard is present but unreadable; not replacing it")
                    return false
                }
                Read.EMPTY -> Log.i(TAG, "user clipboard was empty")
                Read.HAS -> Log.i(TAG, "user clipboard saved")
            }
            saved = prior
            savedPresent = present
            held = true
        }
        val ok = runCatching {
            manager.setPrimaryClip(ClipData.newPlainText(LABEL, text))
            true
        }.getOrElse {
            Log.w(TAG, "could not put text on the clipboard", it)
            false
        }
        if (ok) ours = text
        ok
    }

    /** Put the user's clipboard back, if it is still ours. */
    fun restore(): Boolean = synchronized(lock) {
        if (!held) return true
        val manager = manager
        val ok = manager != null && runCatching {
            val stillOurs = clipText(runCatching { manager.primaryClip }.getOrNull()) == ours
            when (
                ShellClipRestore.decide(
                    stillOurs = stillOurs,
                    savedPresent = savedPresent,
                    savedWasEmpty = saved == null,
                )
            ) {
                Restore.PUT_BACK -> manager.setPrimaryClip(
                    saved?.also { markSensitive(it) } ?: ClipData.newPlainText("", ""),
                )
                Restore.CLEAR -> manager.clearPrimaryClip()
                Restore.LEAVE -> Log.i(TAG, "clipboard was changed by someone else; left alone")
            }
            true
        }.getOrElse {
            Log.w(TAG, "could not restore the clipboard", it)
            false
        }
        saved = null
        savedPresent = false
        ours = null
        held = false
        ok
    }

    private fun present(manager: ClipboardManager): Boolean =
        runCatching { manager.hasPrimaryClip() }.getOrDefault(false) ||
            runCatching { manager.primaryClipDescription != null }.getOrDefault(false)

    private fun clipText(clip: ClipData?): String? =
        clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()

    /** API 33+: this is the user's content, not something to hand around. */
    private fun markSensitive(clip: ClipData) {
        if (Build.VERSION.SDK_INT >= 33) {
            runCatching {
                ClipData::class.java
                    .getMethod("setFlags", Int::class.javaPrimitiveType)
                    .invoke(clip, 1)
            }
        }
    }

    private companion object {
        const val TAG = "SyscityClip"
        const val LABEL = "syscity"
    }
}

/** What the clipboard held. */
internal enum class Read { EMPTY, HAS, UNREADABLE }

/** What a restore should do. */
internal enum class Restore { PUT_BACK, CLEAR, LEAVE }

/** Classify a clipboard read. Split out because this rule is the whole safety story. */
internal object ShellClipRead {
    /**
     * `present` is the platform's "something is there" answer, which it gives
     * even when the text itself does not come back.
     *
     * Present-but-unreadable is the dangerous case: it may be the user's data,
     * and writing over it destroys something that cannot be recovered.
     */
    fun classify(clipTextCameBack: Boolean, present: Boolean): Read = when {
        clipTextCameBack -> Read.HAS
        present -> Read.UNREADABLE
        else -> Read.EMPTY
    }
}

/** What to do with the clipboard once our paste is done. */
internal object ShellClipRestore {
    /**
     * `stillOurs` false means somebody else has copied since; the restore is
     * skipped rather than applied over their content.
     */
    fun decide(stillOurs: Boolean, savedPresent: Boolean, savedWasEmpty: Boolean): Restore = when {
        !stillOurs -> Restore.LEAVE
        savedPresent && !savedWasEmpty -> Restore.PUT_BACK
        else -> Restore.CLEAR
    }
}

/** Whether the paste key may be pressed once the clipboard has been written. */
internal enum class Paste { PROCEED, NO_CLIPBOARD }

/** The rule about pressing Ctrl+V, split out because it decides what text lands. */
internal object ShellPaste {
    /**
     * [putTook] is what `ShellClipboard.put` answered. False means the text is
     * *not* on the clipboard: either the write failed, or the person's
     * clipboard was unreadable and was deliberately left alone.
     *
     * In that case nothing is pressed at all. Appending instead of replacing
     * protects the person's clipboard from being wiped, but it cannot paste
     * text that never reached the clipboard — a Ctrl+V would insert whatever
     * they had copied themselves into the field.
     */
    fun decide(putTook: Boolean): Paste = if (putTook) Paste.PROCEED else Paste.NO_CLIPBOARD
}
