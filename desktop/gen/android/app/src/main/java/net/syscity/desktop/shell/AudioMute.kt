package net.syscity.desktop.shell

import android.util.Log
import java.io.File

/**
 * Which apps this helper has silenced, and what their sound was set to first.
 *
 * An app the agent opened on its second screen is an app the user cannot see.
 * Whatever it plays — a notification chime, a video started by a stray tap, a
 * voice note — comes out of a phone showing something else, and there is no way
 * for the person holding it to connect the sound to anything. Silencing the app
 * while it sits on the agent's screen is the difference between a phone that is
 * being used and a phone that is possessed.
 *
 * `appops set <pkg> PLAY_AUDIO ignore` is the switch, and it is per-package and
 * persistent, so the rules that make this safe to do to somebody's phone are:
 *
 * - **only the packages recorded here are ever written**, so a mode the user
 *   set for their own reasons is never touched; and
 * - **the mode read before silencing is written back**, because "put the sound
 *   back" must mean what it was, not what we assume is normal. An app that was
 *   already silenced stays silenced.
 * - **the same app on two displays is counted, not re-recorded**: the first
 *   mute remembers the prior mode, the second only raises the count, and the
 *   sound returns when the last display showing it goes away.
 *
 * The record is a file as well as a map: this process is started fresh every
 * session and can be killed at any moment, and a mute that outlives the process
 * that set it is a permanently silent app. On start-up the leftovers are
 * restored (see `HelperMain`) — the file is the only thing that survives. It
 * lives in `/data/local/tmp` (see `HelperMain.stateDir`): not in `tmpdir`,
 * which under `app_process` is a tmpfs and a reboot would take the record with
 * it.
 *
 * The file names the process that wrote it, and only that process's death makes
 * the record inheritable. That is not bookkeeping: a second helper can start
 * while the first is still alive — the app decides a helper is missing and
 * starts one — and if the newcomer restored the file it would be undoing a
 * mute whose display is still up, leaving an app the user cannot see that is
 * also somehow audible. So a live owner's file is left alone: the newcomer
 * keeps its own record in memory, loses only the ability to survive its own
 * death, and warns about it.
 *
 * When the file cannot be written the mute still works and simply is not
 * recoverable after a kill: a session that loses its voice forever is worse
 * than one that loses the record.
 */
internal class AudioMuteLedger(
    private val file: File?,
    private val warn: (String) -> Unit = { Log.w(TAG, it) },
    /** This process, as the record's owner line spells it; empty when unknown. */
    private val owner: String = processOwner().orEmpty(),
    /** Whether a process that wrote a record is still running; see [isAlive]. */
    private val alive: (String) -> Boolean = Companion::isAlive,
) {

    /** Prior mode and how many displays still want this package silent. */
    internal data class Entry(val prior: String, var count: Int)

    private val entries = LinkedHashMap<String, Entry>()

    /**
     * A file this process may not write: it belongs to a helper that is still
     * alive, and restoring or overwriting it would undo a mute whose display
     * is still up.
     */
    private val borrowed: File? = run {
        val previous = parseOwner(record()) ?: return@run null
        if (previous == owner || !alive(previous)) return@run null
        warn(
            "another helper (${previous.substringBefore(' ')}) is still recording " +
                "audio mutes; this one will not restore or overwrite its record",
        )
        file
    }

    /** Whether the record on disk is ours to read, write, and restore. */
    private val inherited: Boolean = borrowed == null

    init {
        // A record left by a dead helper — including one from a version before
        // the owner line existed — is inherited: it is the only thing that
        // will ever give those apps their sound back.
        if (inherited) {
            parse(record()).forEach { (pkg, entry) -> entries[pkg] = Entry(entry.prior, entry.count) }
        }
    }

    private fun record(): String =
        runCatching { file?.takeIf { it.isFile }?.readText() }.getOrNull().orEmpty()

    fun contains(packageName: String): Boolean = synchronized(entries) { packageName in entries }

    /** The mode recorded before this package was silenced, or null. */
    operator fun get(packageName: String): String? = synchronized(entries) { entries[packageName]?.prior }

    fun packages(): List<String> = synchronized(entries) { entries.keys.toList() }

    /**
     * Whether a record on disk belongs to somebody else, and so should not be
     * restored at start-up. False when there is no file, or it is unreadable —
     * an unreadable file is a file this process could not have written either.
     */
    fun ownedByAnother(): Boolean = !inherited

    /**
     * One more display wants `packageName` silent, remembering [prior] the
     * first time. Returns whether this was the first record.
     */
    fun acquire(packageName: String, prior: String): Boolean {
        val first = synchronized(entries) {
            val entry = entries[packageName]
            if (entry == null) {
                entries[packageName] = Entry(prior, 1)
                true
            } else {
                entry.count += 1
                false
            }
        }
        save()
        return first
    }

    /** One display no longer does. Returns true when that was the last one. */
    fun release(packageName: String): Boolean {
        val last = synchronized(entries) {
            val entry = entries[packageName] ?: return false
            entry.count -= 1
            if (entry.count > 0) return false
            entries.remove(packageName)
            true
        }
        save()
        return last
    }

    /** Forget a package outright — used when the mute itself failed. */
    fun forget(packageName: String) {
        val removed = synchronized(entries) { entries.remove(packageName) != null }
        if (removed) save()
    }

    /** Restore every record; whatever restores successfully is forgotten. */
    fun restoreAll(restore: (packageName: String, mode: String) -> Boolean): Boolean {
        packages().forEach { pkg ->
            val mode = this[pkg] ?: return@forEach
            if (restore(pkg, mode)) forget(pkg)
        }
        return packages().isEmpty()
    }

    private fun save() {
        val f = file ?: return
        // Another live helper owns the file. Writing it would hand our records
        // to a process that is about to restore them out from under a display
        // it never opened.
        if (borrowed != null) return
        val body = synchronized(entries) { format(entries.mapValues { it.value.prior to it.value.count }) }
        runCatching {
            if (body.isEmpty()) {
                f.delete()
            } else {
                f.parentFile?.mkdirs()
                // Write-then-rename: a kill between truncate and write would
                // leave a half-written record, and a half-written record is a
                // package that is never restored. `renameTo` will not replace
                // an existing file on every filesystem, so the old one goes
                // first and the retry is what turns a lost record into an
                // error somebody sees.
                val tmp = File(f.parentFile, f.name + ".tmp")
                tmp.writeText(formatWithOwner(owner, body))
                var renamed = tmp.renameTo(f)
                if (!renamed) {
                    f.delete()
                    renamed = tmp.renameTo(f)
                }
                if (!renamed) error("rename onto ${f.path} failed")
                Unit
            }
        }.onFailure { warn("could not persist the audio-mute record: ${it.message}") }
    }

    companion object {
        private const val TAG = "SyscityShell"

        /** The process that wrote the record: `<pid> <starttime>`. */
        private const val OWNER_PREFIX = "owner "

        /** `<package> <mode> <count>` per line. */
        private val LINE = Regex("""([A-Za-z0-9_.]+) ([a-z_]+)(?: (\d+))?""")

        /**
         * Whether the helper that wrote `owner` may still be running.
         *
         * A pid alone is not enough: pids are reused, and the pid that wrote a
         * record a reboot ago may belong to an unrelated process now. The start
         * time (`/proc/<pid>/stat` field 22, in clock ticks since boot) pins the
         * same process. A pid that no longer exists is gone, whichever answer
         * the start time would give.
         */
        private fun isAlive(owner: String): Boolean {
            val pid = owner.substringBefore(' ').toIntOrNull() ?: return false
            val started = owner.substringAfter(' ', "").toLongOrNull()
            if (started == null) {
                // An owner line from a format this version does not know: assume
                // live, because restoring on a guess is the failure that cannot
                // be undone.
                return true
            }
            val stat = runCatching { File("/proc/$pid/stat").readText() }.getOrNull() ?: return false
            return fieldsOf(stat)?.getOrNull(19)?.toLongOrNull() == started
        }

        internal fun format(entries: Map<String, Pair<String, Int>>): String =
            entries.entries.joinToString("") { (pkg, value) ->
                "${pkg} ${value.first} ${value.second}\n"
            }

        internal fun formatWithOwner(owner: String, body: String): String =
            owner.takeIf(String::isNotEmpty)?.let { "$OWNER_PREFIX$it\n$body" } ?: body

        /**
         * This process's identity, as `/proc/self/stat` spells it:
         * `<pid> <starttime>`, where the start time is field 22 — clock ticks
         * since boot.
         *
         * The pid alone is not an identity: pids are reused, and the pid that
         * wrote a record a reboot ago may belong to an unrelated process now.
         * The start time pins the same process, and reading both from the same
         * file is what makes them comparable. Null when `/proc` is not there,
         * in which case the record simply carries no owner.
         */
        internal fun processOwner(): String? {
            val stat = runCatching { File("/proc/self/stat").readText() }.getOrNull() ?: return null
            val fields = fieldsOf(stat) ?: return null
            val pid = stat.substringBefore(' ').trim().toIntOrNull() ?: return null
            // Field 3 of `stat` (state) is `fields[0]` here, so starttime (22)
            // is `fields[19]`.
            val started = fields.getOrNull(19)?.toLongOrNull() ?: return null
            return "$pid $started"
        }

        /**
         * `/proc/<pid>/stat` after the command name: the name is in
         * parentheses, may contain spaces and parentheses of its own, so the
         * fields are counted from the last `)`.
         */
        private fun fieldsOf(stat: String): List<String>? =
            stat.substringAfterLast(')', "")
                .takeIf { it.isNotEmpty() }
                ?.trim()
                ?.split(' ')
                ?.takeIf { it.size > 19 }

        /** The owner line of a record, or null when it has none. */
        internal fun parseOwner(text: String): String? =
            text.lineSequence()
                .firstOrNull { it.startsWith(OWNER_PREFIX) }
                ?.removePrefix(OWNER_PREFIX)
                ?.trim()
                ?.takeIf(String::isNotEmpty)

        internal fun parse(text: String): Map<String, Entry> =
            text.lineSequence()
                .mapNotNull { LINE.matchEntire(it.trim())?.destructured }
                .associate { (pkg, mode, count) ->
                    pkg to Entry(mode, count.toIntOrNull()?.coerceAtLeast(1) ?: 1)
                }
    }
}

/**
 * The `PLAY_AUDIO` app-op, as text.
 *
 * `appops get <pkg> PLAY_AUDIO` prints one or two lines depending on what has
 * been set:
 *
 * ```
 * Uid mode: PLAY_AUDIO: default      # the whole uid's mode, always present
 * PLAY_AUDIO: ignore                 # this package's mode, only when set
 * ```
 *
 * or, for a package with nothing set at all, `No operations.` — in which case
 * the effective mode is the system default, which is `allow`.
 *
 * A parse that took the first match would read the uid line and believe a
 * silenced package was never set; one that took the last would be right by
 * accident. The package line is the one that matters and is looked for first.
 */
internal object PlayAudio {
    const val OP = "PLAY_AUDIO"
    const val IGNORE = "ignore"
    const val ALLOW = "allow"
    const val DEFAULT = "default"

    private val PACKAGE_LINE = Regex("""^PLAY_AUDIO:\s*([a-z_]+)""")
    private val UID_LINE = Regex("""^Uid mode:\s*PLAY_AUDIO:\s*([a-z_]+)""")

    /** The mode this package is in, or null when nothing has been set. */
    fun parse(output: String): String? {
        val lines = output.lineSequence().map(String::trim).filter(String::isNotEmpty).toList()
        for (line in lines) PACKAGE_LINE.find(line)?.let { return it.groupValues[1] }
        for (line in lines) UID_LINE.find(line)?.let { return it.groupValues[1] }
        return null
    }

    /** The mode to write to put the sound back: what it was, or "nothing set". */
    fun restoreMode(prior: String?): String = prior ?: DEFAULT

    /** Whether what was read back after a restore still means "no sound". */
    fun stillSilent(after: String?): Boolean = after == IGNORE
}

/**
 * Silencing an app, and putting its sound back.
 *
 * Only the helper can do this: `appops` needs the shell identity, which is the
 * whole reason the helper exists. The command runner is injectable so the
 * sequence — read the prior mode, record it, write, read back — can be tested
 * without a phone.
 */
internal class AudioControl(
    private val ledger: AudioMuteLedger,
    private val userId: Int,
    private val exec: (Array<String>) -> Pair<Int, String> = ::runAppOps,
    private val warn: (String) -> Unit = { Log.w(TAG, it) },
) {

    /**
     * Whether the record on disk belongs to a helper that is still running, and
     * so must not be restored at start-up. False when there is no record, or it
     * is unreadable — an unreadable record is one this process could not have
     * written either.
     */
    fun ownedByAnother(): Boolean = ledger.ownedByAnother()

    /** This package's current `PLAY_AUDIO` mode, or null when it cannot be read. */
    @Synchronized
    fun mode(packageName: String): String? {
        val (code, out) = appops("get", packageName)
        return if (code == 0) PlayAudio.parse(out) else null
    }

    /**
     * Silence `packageName` for as long as one more display shows it.
     *
     * Returns false — with nothing recorded — when the current mode cannot be
     * read first: muting without knowing what to restore is how an app ends up
     * silent forever.
     */
    @Synchronized
    fun mute(packageName: String): Boolean {
        if (ledger.contains(packageName)) {
            // Somebody already silenced it; only the count changes.
            ledger.acquire(packageName, PlayAudio.restoreMode(null))
            return true
        }
        val (code, out) = appops("get", packageName)
        if (code != 0) {
            warn("cannot read ${PlayAudio.OP} for $packageName; not silencing it")
            return false
        }
        val prior = PlayAudio.parse(out) ?: PlayAudio.DEFAULT
        ledger.acquire(packageName, prior)
        if (appops("set", packageName, PlayAudio.IGNORE).first != 0) {
            ledger.forget(packageName)
            warn("could not silence $packageName")
            return false
        }
        return true
    }

    /**
     * One display stopped showing `packageName`; give the sound back when the
     * last one has. A package that was never silenced is already restored.
     */
    @Synchronized
    fun restore(packageName: String): Boolean {
        val prior = ledger[packageName] ?: return true
        if (!ledger.release(packageName)) return true
        return writeBack(packageName, prior)
    }

    /** Put every recorded app's sound back. Used at start-up and at teardown. */
    @Synchronized
    fun restoreAll(): Boolean =
        ledger.restoreAll { pkg, mode -> writeBack(pkg, mode) }

    /**
     * Write the prior mode and check it took.
     *
     * `default` is what "nothing was set" means, but on some builds this op is
     * stored per-uid and a plain write leaves the package line reading
     * `ignore`; when that happens the package is set to `allow` explicitly,
     * which is what `default` means for this op anyway.
     */
    private fun writeBack(packageName: String, prior: String): Boolean {
        val write = appops("set", packageName, PlayAudio.restoreMode(prior)).first == 0
        if (!write) {
            warn("could not restore the sound of $packageName")
            return false
        }
        if (PlayAudio.stillSilent(mode(packageName))) {
            if (appops("set", packageName, PlayAudio.ALLOW).first != 0) {
                warn("could not restore the sound of $packageName")
                return false
            }
        }
        return true
    }

    /** `appops <verb> --user <id> <package> PLAY_AUDIO [mode]` */
    private fun appops(verb: String, packageName: String, mode: String? = null): Pair<Int, String> {
        val command = mutableListOf(
            "appops", verb, "--user", userId.toString(), packageName, PlayAudio.OP,
        )
        if (mode != null) command.add(mode)
        return exec(command.toTypedArray())
    }

    private companion object {
        const val TAG = "SyscityShell"
    }
}

/** Run one `appops` command as this process (uid 2000, the shell). */
private fun runAppOps(command: Array<String>): Pair<Int, String> = runCatching {
    val process = ProcessBuilder(*command).redirectErrorStream(true).start()
    val text = process.inputStream.bufferedReader().use { it.readText() }
    process.waitFor() to text
}.getOrElse { (-1) to (it.message ?: "appops did not run") }
