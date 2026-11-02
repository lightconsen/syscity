package net.syscity.desktop.shell

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * The parts of the phone control that are pure logic.
 *
 * Everything Android-specific lives behind them, so the rules that decide
 * "is this a change" and "may I touch the clipboard" can be tested on a JVM
 * instead of only on a device — which is the difference between a rule that is
 * checked on every build and one that is checked when somebody remembers.
 */
class PhoneLogicTest {

    // ── The clipboard ──────────────────────────────────────────────────

    @Test
    fun a_readableClipboardIsSaved() {
        assertEquals(Read.HAS, ShellClipRead.classify(clipTextCameBack = true, present = true))
    }

    @Test
    fun anEmptyClipboardIsSafeToWriteTo() {
        assertEquals(Read.EMPTY, ShellClipRead.classify(clipTextCameBack = false, present = false))
    }

    @Test
    fun aClipboardThatIsPresentButUnreadableIsNeverReplaced() {
        // The platform sometimes withholds the text while still saying there is
        // something there. Writing over it would destroy what cannot be read.
        assertEquals(
            Read.UNREADABLE,
            ShellClipRead.classify(clipTextCameBack = false, present = true),
        )
    }

    @Test
    fun theUsersOwnClipboardIsPutBack() {
        assertEquals(
            Restore.PUT_BACK,
            ShellClipRestore.decide(stillOurs = true, savedPresent = true, savedWasEmpty = false),
        )
    }

    @Test
    fun anOriginallyEmptyClipboardIsClearedRatherThanFilled() {
        assertEquals(
            Restore.CLEAR,
            ShellClipRestore.decide(stillOurs = true, savedPresent = true, savedWasEmpty = true),
        )
    }

    @Test
    fun aClipboardSomebodyElseChangedIsLeftAlone() {
        // The user copied something of their own mid-paste; putting the old
        // contents back would delete it.
        assertEquals(
            Restore.LEAVE,
            ShellClipRestore.decide(stillOurs = false, savedPresent = true, savedWasEmpty = false),
        )
    }

    // ── Reading the task list ──────────────────────────────────────────

    private val header = "ACTIVITY MANAGER ACTIVITIES (dumpsys activity activities)"

    @Test
    fun eachTaskKeepsTheDisplayItWasListedUnder() {
        val dump = """
            $header
            Display #0 (activities from top to bottom):

              * Task{da0b817 #161 type=standard A=10168:com.photos U=0}
                topResumedActivity=ActivityRecord{7c6496 u0 com.photos/.Home t161}
              * Task{1b80ef0 #1 type=home A=10177:com.launcher U=0}

            Display #7 (activities from top to bottom):

              * Task{7345ed9 #160 type=standard A=1000:com.settings U=0}
                topResumedActivity=ActivityRecord{2564c23 u0 com.settings/.Settings t160}
        """.trimIndent()

        val tasks = TaskQuery.parse(dump).associateBy { it.taskId }
        assertEquals(0, tasks.getValue(161).displayId)
        assertEquals(0, tasks.getValue(1).displayId)
        assertEquals(7, tasks.getValue(160).displayId)
    }

    @Test
    fun theSupervisorStateDoesNotMoveTasksToTheLastDisplaySeen() {
        // The real dump repeats every task under `ActivityTaskSupervisor
        // state:` / `Application tokens in top down Z order`. Reading that far
        // put the user's task on the agent's display, and a real spill then
        // looked like nothing had happened.
        val dump = """
            $header
            Display #0 (activities from top to bottom):

              * Task{da0b817 #161 type=standard A=10168:com.photos U=0}
                topResumedActivity=ActivityRecord{7c6496 u0 com.photos/.Home t161}

            Display #7 (activities from top to bottom):

              * Task{7345ed9 #160 type=standard A=1000:com.settings U=0}
                topResumedActivity=ActivityRecord{2564c23 u0 com.settings/.Settings t160}

            ActivityTaskSupervisor state:
              topDisplayFocusedRootTask=Task{da0b817 #161 type=standard A=10168:com.photos}
              Task display areas in top down Z order:
                TaskDisplayArea DefaultTaskDisplayArea
                  Application tokens in top down Z order:
                  * Task{da0b817 #161 type=standard A=10168:com.photos U=0}
                    bounds=[0,0][1080,2400]
                  * Task{7345ed9 #160 type=standard A=1000:com.settings U=0}
                    bounds=[0,0][1080,2400]
        """.trimIndent()

        val tasks = TaskQuery.parse(dump)
        assertEquals("the repeated listing must not add tasks: $tasks", 2, tasks.size)
        assertEquals(0, tasks.first { it.taskId == 161 }.displayId)
    }

    @Test
    fun aTaskThatIsNotAStandardAppIsMarkedSo() {
        val dump = """
            $header
            Display #0 (activities from top to bottom):

              * Task{1b80ef0 #1 type=home I=com.android.launcher/.Home U=0}
              * Task{58c006b #3 type=undefined U=0}
        """.trimIndent()

        val tasks = TaskQuery.parse(dump).associateBy { it.taskId }
        assertFalse(tasks.getValue(1).standard)
        assertFalse(
            "a task with no package is not an app page",
            tasks.getValue(3).standard,
        )
    }

    @Test
    fun theShareSheetsResolverIsNamedFromItsIntent() {
        // The resolver names its component as `I=<pkg>/<class>` on the task
        // line and has no `A=`; missing it hid a real spill.
        val dump = """
            $header
            Display #0 (activities from top to bottom):

              * Task{42 #42 type=standard I=com.android.intentresolver/.ChooserActivity U=0}
        """.trimIndent()

        assertEquals(
            "com.android.intentresolver",
            TaskQuery.parse(dump).single().packageName,
        )
    }

    @Test
    fun aTaskAffinityThatIsAClassDoesNotBecomeThePackage() {
        // Calendar's affinity is `com.android.calendar.event.LaunchInfoActivity`;
        // reporting that as the app named a page, not the app.
        val dump = """
            $header
            Display #0 (activities from top to bottom):

              * Task{4fb1526 #184 type=standard A=10161:com.android.calendar.event.LaunchInfoActivity U=0}
                topResumedActivity=ActivityRecord{2564c23 u0 com.google.android.calendar/com.android.calendar.event.LaunchInfoActivity t184}
        """.trimIndent()

        val task = TaskQuery.parse(dump).single()
        assertEquals("com.google.android.calendar", task.packageName)
        assertEquals(
            "com.google.android.calendar/com.android.calendar.event.LaunchInfoActivity",
            task.topActivity,
        )
    }

    @Test
    fun theResumedActivityIsTheNameEvenWhenTheAffinityIsThePackage() {
        val dump = """
            $header
            Display #0 (activities from top to bottom):

              * Task{7345ed9 #160 type=standard A=1000:com.android.settings.root U=0}
                topResumedActivity=ActivityRecord{2564c23 u0 com.android.settings/.Settings t160}
        """.trimIndent()

        val task = TaskQuery.parse(dump).single()
        assertEquals("com.android.settings", task.packageName)
        assertEquals("com.android.settings/.Settings", task.topActivity)
    }

    // ── Main-screen arrivals ───────────────────────────────────────────

    private fun task(id: Int, display: Int, pkg: String?, activity: String? = null) =
        MainScreenLog.Task(id, display, pkg, activity ?: "$pkg.Main", standard = true)

    @Test
    fun aTaskThatRisesToTheTopIsRecordedOnce() {
        val log = MainScreenLog()
        // Somebody else is on the user's screen when watching starts.
        log.scan(listOf(task(2, 0, "com.b")), now = 100, record = false)
        // Our app rises on the agent's display (the user's screen is untouched),
        // then the system puts it on the user's screen.
        log.scan(listOf(task(1, 3, "com.a"), task(2, 0, "com.b")), now = 150, record = true)
        log.scan(listOf(task(1, 0, "com.a")), now = 300, record = true)

        val arrivals = log.since(0)
        assertEquals(1, arrivals.size)
        assertEquals(1, arrivals[0].taskId)
        // It was last seen on the agent's own display (3), which is what marks
        // it as having been moved rather than opened from nowhere.
        assertEquals(3, arrivals[0].from)
    }

    @Test
    fun theSameTaskChangingPageIsRecorded() {
        val log = MainScreenLog()
        log.scan(listOf(task(1, 0, "com.a", "com.a.Home")), now = 100, record = false)
        log.scan(listOf(task(1, 0, "com.a", "com.a.Settings")), now = 200, record = true)

        val arrivals = log.since(0)
        assertEquals(1, arrivals.size)
        assertTrue(arrivals[0].sameTask)
        assertEquals(MainScreenLog.MAIN, arrivals[0].from)
    }

    @Test
    fun aBaselineScanRecordsNothing() {
        // What changed before watching started cannot be attributed to an action.
        val log = MainScreenLog()
        log.scan(listOf(task(1, 0, "com.a")), now = 100, record = false)
        log.scan(listOf(task(2, 0, "com.b")), now = 200, record = false)
        assertTrue(log.since(0).isEmpty())
    }

    @Test
    fun aTaskThatLeavesTheUsersScreenIsReportedAsGone() {
        val log = MainScreenLog()
        log.scan(listOf(task(1, 0, "com.a")), now = 100, record = false)
        log.scan(listOf(task(2, 0, "com.b"), task(1, 3, "com.a")), now = 200, record = true)
        assertEquals(3, log.displayOf(1))
        assertEquals(-1, log.displayOf(99))
    }

    @Test
    fun oldArrivalsAreDropped() {
        val log = MainScreenLog(keepMs = 1_000)
        log.scan(listOf(task(1, 0, "com.a")), now = 0, record = false)
        log.scan(listOf(task(2, 0, "com.b")), now = 100, record = true)
        log.scan(listOf(task(3, 0, "com.c")), now = 5_000, record = true)
        val tasks = log.since(0).map { it.taskId }
        assertEquals(listOf(3), tasks)
    }

    // ── Parallel limit ─────────────────────────────────────────────────

    @Test
    fun aPhoneBelowAndroid14RunsOneDisplay() {
        val (limit, reason) = ParallelLimit.of(sdk = 33, lowRam = false, secondDisplayFailed = false)
        assertEquals(1, limit)
        assertTrue(reason!!.contains(ParallelLimit.MIN_SDK.toString()))
    }

    @Test
    fun aLowMemoryPhoneRunsOneDisplay() {
        val (limit, reason) = ParallelLimit.of(sdk = 35, lowRam = true, secondDisplayFailed = false)
        assertEquals(1, limit)
        assertTrue(reason!!.contains("low memory"))
    }

    @Test
    fun aDisplayThatFailedOnceIsNotRetried() {
        val (limit, reason) = ParallelLimit.of(sdk = 35, lowRam = false, secondDisplayFailed = true)
        assertEquals(1, limit)
        assertTrue(reason!!.contains("failed"))
    }

    @Test
    fun aCapablePhoneGetsTheMaximum() {
        val (limit, reason) = ParallelLimit.of(sdk = 35, lowRam = false, secondDisplayFailed = false)
        assertEquals(ParallelLimit.MAX, limit)
        assertNull(reason)
        assertEquals(0, ParallelLimit.remaining(open = 3, limit = 3))
        assertEquals(2, ParallelLimit.remaining(open = 1, limit = 3))
    }

    // ── Frame signature ────────────────────────────────────────────────

    private fun solid(width: Int, height: Int, argb: Int): ByteArray =
        FrameSignature.of(width, height) { _, _ -> argb }

    @Test
    fun aSignatureCarriesItsGridSize() {
        val signature = solid(240, 480, 0xFF808080.toInt())
        assertEquals(FrameSignature.GRID_W.toByte(), signature[0])
        assertEquals(FrameSignature.GRID_H.toByte(), signature[1])
        assertEquals(2 + FrameSignature.GRID_W * FrameSignature.GRID_H, signature.size)
    }

    @Test
    fun aUniformScreenHasOneValueInEveryCell() {
        val signature = solid(240, 480, 0xFFFFFFFF.toInt())
        val cells = signature.drop(2)
        assertTrue(cells.all { it == 0x40.toByte() || it.toInt() and 0xff == 255 })
    }

    @Test
    fun aBlackScreenAndAWhiteScreenDiffer() {
        val black = solid(240, 480, 0xFF000000.toInt())
        val white = solid(240, 480, 0xFFFFFFFF.toInt())
        val changed = black.drop(2).zip(white.drop(2)).count { (a, b) ->
            Math.abs((a.toInt() and 0xff) - (b.toInt() and 0xff)) > FrameSignature.CELL_DELTA
        }
        assertTrue(changed >= FrameSignature.MIN_CELLS)
    }

    // ── Gesture sequencing ─────────────────────────────────────────────

    @Test
    fun theFirstEventOfAGestureNamesOnePointer() {
        // The dispatcher refuses `invalid pointer count 2` on ACTION_DOWN, so a
        // second finger may never be present in the first event.
        val plan = GesturePlan.of(fingers = 2, steps = 3)
        assertEquals(GesturePlan.ACTION_DOWN, plan.first().action)
        assertEquals(1, plan.first().count)
    }

    @Test
    fun theSecondFingerArrivesAsAPointerDown() {
        val plan = GesturePlan.of(fingers = 2, steps = 3)
        val second = plan[1]
        assertEquals(GesturePlan.ACTION_POINTER_DOWN, second.action)
        assertEquals(2, second.count)
        assertEquals(1, second.actionIndex)
    }

    @Test
    fun fingersLeaveInReverseOrder() {
        val plan = GesturePlan.of(fingers = 2, steps = 3)
        val lifts = plan.filter {
            it.action == GesturePlan.ACTION_POINTER_UP || it.action == GesturePlan.ACTION_UP
        }
        assertEquals(GesturePlan.ACTION_POINTER_UP, lifts[0].action)
        assertEquals(1, lifts[0].actionIndex)
        assertEquals(GesturePlan.ACTION_UP, lifts[1].action)
        assertEquals(1, lifts[1].count)
    }

    @Test
    fun everyMoveEventNamesEveryFinger() {
        val plan = GesturePlan.of(fingers = 2, steps = 4)
        val moves = plan.filter { it.action == GesturePlan.ACTION_MOVE }
        assertEquals(3, moves.size)
        assertTrue(moves.all { it.count == 2 })
    }

    @Test
    fun aGestureWithNoMovementIsASinglePlacedFinger() {
        val plan = GesturePlan.of(fingers = 1, steps = 1)
        assertEquals(
            listOf(GesturePlan.ACTION_DOWN, GesturePlan.ACTION_UP),
            plan.map { it.action },
        )
    }

    @Test
    fun aFingerThatStopsMovingHoldsItsLastPoint() {
        assertEquals(4, GesturePlan.positionFor(position = 9, pathLength = 5))
        assertEquals(2, GesturePlan.positionFor(position = 2, pathLength = 5))
    }

    @Test
    fun anEmptyPathNeverProducesANegativePosition() {
        assertEquals(0, GesturePlan.positionFor(position = 3, pathLength = 0))
    }

    @Test
    fun thePlanAgreesWithTheActionsAndroidDefines() {
        // These are the platform's own values; a copy that drifted would build
        // events the dispatcher silently reads as something else.
        assertEquals(0, GesturePlan.ACTION_DOWN)
        assertEquals(1, GesturePlan.ACTION_UP)
        assertEquals(2, GesturePlan.ACTION_MOVE)
        assertEquals(5, GesturePlan.ACTION_POINTER_DOWN)
        assertEquals(6, GesturePlan.ACTION_POINTER_UP)
    }

    // ── Silencing an app on the agent's display ─────────────────────────

    @Test
    fun aPackageLineBeatsTheUidLine() {
        // A silenced package prints both; the uid line says `default` and would
        // make a muted app look untouched if it were read first.
        assertEquals(
            PlayAudio.IGNORE,
            PlayAudio.parse("Uid mode: PLAY_AUDIO: default\nPLAY_AUDIO: ignore"),
        )
    }

    @Test
    fun aUidOnlyAnswerStillSaysSomething() {
        assertEquals(PlayAudio.IGNORE, PlayAudio.parse("Uid mode: PLAY_AUDIO: ignore"))
    }

    @Test
    fun aPackageWithNothingSetReadsAsNull() {
        // "No operations." is the answer for an app nobody has set: the mode is
        // the system default, and there is nothing to write back.
        assertNull(PlayAudio.parse("No operations."))
        assertNull(PlayAudio.parse(""))
    }

    @Test
    fun restoringAnAppWithNothingSetWritesDefaultNotAllow() {
        // `allow` would be a mode the user did not choose; `default` is what
        // "nothing was set" means and is what the package line came from.
        assertEquals(PlayAudio.DEFAULT, PlayAudio.restoreMode(null))
        assertEquals(PlayAudio.IGNORE, PlayAudio.restoreMode(PlayAudio.IGNORE))
    }

    @Test
    fun aLedgerLineRoundTrips() {
        val text = AudioMuteLedger.format(
            linkedMapOf("com.a" to (PlayAudio.DEFAULT to 2), "com.b.c" to (PlayAudio.IGNORE to 1)),
        )
        val parsed = AudioMuteLedger.parse(text)
        assertEquals("default", parsed["com.a"]?.prior)
        assertEquals(2, parsed["com.a"]?.count)
        assertEquals("ignore", parsed["com.b.c"]?.prior)
    }

    @Test
    fun aHalfWrittenLineIsIgnoredRatherThanMisread() {
        // A record that cannot be read is a package that is never restored, so
        // an unknown line is dropped and the rest of the file still counts.
        val parsed = AudioMuteLedger.parse("com.a default 1\ncom.b\n\n  noise  \n")
        assertEquals(setOf("com.a"), parsed.keys)
    }

    @Test
    fun theOwnerLineNamesTheProcessThatWroteTheRecord() {
        val text = "owner 4242 99123\ncom.a default 1\n"
        assertEquals("4242 99123", AudioMuteLedger.parseOwner(text))
        assertNull(AudioMuteLedger.parseOwner("com.a default 1\n"))
    }

    @Test
    fun aRecordLeftByADeadHelperIsInherited() {
        // The pid is gone, so nothing is holding those mutes: restoring them at
        // start-up is the only thing that will ever give the sound back.
        val file = tempFile("owner 999999 1\ncom.a default 1\n")
        val ledger = AudioMuteLedger(file = file, warn = {}, owner = "1 1", alive = { false })
        assertFalse(ledger.ownedByAnother())
        assertTrue(ledger.contains("com.a"))
        assertEquals("default", ledger["com.a"])
    }

    @Test
    fun aRecordOwnedByALiveHelperIsLeftAlone() {
        // Two helpers can be alive at once. The newcomer did not open these
        // displays and must not write to or outlive their record: restoring it
        // would make an app the user cannot see audible again.
        val text = "owner 4242 99123\ncom.a default 1\n"
        val file = tempFile(text)
        val ours = AudioMuteLedger(
            file = file,
            warn = {},
            owner = "1 1",
            alive = { it == "4242 99123" },
        )
        assertTrue(ours.ownedByAnother())
        assertFalse(ours.contains("com.a"))
        ours.acquire("com.b", PlayAudio.DEFAULT)
        assertTrue(ours.contains("com.b"))
        // Our record stays in memory; the other helper's file is untouched.
        assertEquals(text, file.readText())
    }

    @Test
    fun aRecordWithoutAnOwnerLineIsTakenRatherThanAbandoned() {
        // Written by a version before the owner line existed: no process can be
        // asked, and taking it is the only reading that restores the apps.
        val file = tempFile("com.a default 1\n")
        val ledger = AudioMuteLedger(file = file, warn = {}, owner = "1 1", alive = { false })
        assertFalse(ledger.ownedByAnother())
        assertTrue(ledger.contains("com.a"))
    }

    @Test
    fun aRecordThisProcessWroteIsInherited() {
        // Restarting inside the same pid is not possible, but the comparison is
        // what protects a live helper: an equal owner is this process's own file.
        val file = tempFile("owner 1 1\ncom.a default 1\n")
        val ledger = AudioMuteLedger(file = file, warn = {}, owner = "1 1", alive = { true })
        assertFalse(ledger.ownedByAnother())
        assertTrue(ledger.contains("com.a"))
    }

    private fun tempFile(text: String): File {
        val file = File.createTempFile("audio_mute_test", ".txt")
        file.deleteOnExit()
        file.writeText(text)
        return file
    }

    @Test
    fun twoDisplaysShowingTheSameAppReleaseItOnce() {
        val ledger = AudioMuteLedger(file = null, warn = {})
        assertTrue(ledger.acquire("com.x", PlayAudio.DEFAULT))
        assertFalse(ledger.acquire("com.x", PlayAudio.DEFAULT))
        assertFalse(ledger.release("com.x"))
        assertTrue(ledger.release("com.x"))
        assertFalse(ledger.contains("com.x"))
    }

    @Test
    fun mutingRemembersWhatTheSoundWasBefore() {
        val commands = mutableListOf<String>()
        val control = AudioControl(
            ledger = AudioMuteLedger(file = null, warn = {}),
            userId = 0,
            exec = { command ->
                commands.add(command.joinToString(" "))
                when {
                    command[1] == "get" -> 0 to "Uid mode: PLAY_AUDIO: default"
                    else -> 0 to ""
                }
            },
            warn = {},
        )
        assertTrue(control.mute("com.x"))
        // Read first, then write: a mute whose prior mode is unknown must not
        // happen at all.
        assertEquals(
            listOf(
                "appops get --user 0 com.x PLAY_AUDIO",
                "appops set --user 0 com.x PLAY_AUDIO ignore",
            ),
            commands,
        )
        assertEquals(PlayAudio.DEFAULT, control.mode("com.x"))
    }

    @Test
    fun aMuteItCannotReadFirstIsNotWritten() {
        val commands = mutableListOf<String>()
        val control = AudioControl(
            ledger = AudioMuteLedger(file = null, warn = {}),
            userId = 0,
            exec = { command ->
                commands.add(command.joinToString(" "))
                255 to "Error: No UID for com.x in user 0"
            },
            warn = {},
        )
        assertFalse(control.mute("com.x"))
        assertEquals(1, commands.size)
        assertTrue(commands.single().startsWith("appops get"))
    }

    @Test
    fun anAppThatRefusesToBeSilencedIsNotRecorded() {
        val control = AudioControl(
            ledger = AudioMuteLedger(file = null, warn = {}),
            userId = 0,
            exec = { command ->
                if (command[1] == "get") 0 to "PLAY_AUDIO: default" else 1 to "denied"
            },
            warn = {},
        )
        assertFalse(control.mute("com.x"))
        // Nothing was recorded, so `restore` finds nothing to write — which is
        // true, and is the whole point: a later `sound` cannot write over a
        // mode the user set themselves.
        assertTrue(control.restore("com.x"))
    }

    @Test
    fun restoringWritesTheModeThatWasThereAndChecksItTook() {
        val commands = mutableListOf<String>()
        val control = AudioControl(
            ledger = AudioMuteLedger(file = null, warn = {}),
            userId = 10,
            exec = { command ->
                commands.add(command.joinToString(" "))
                when {
                    command[1] == "get" && commands.size == 1 -> 0 to "PLAY_AUDIO: allow"
                    command[1] == "get" -> 0 to "PLAY_AUDIO: allow"
                    else -> 0 to ""
                }
            },
            warn = {},
        )
        assertTrue(control.mute("com.x"))
        commands.clear()
        assertTrue(control.restore("com.x"))
        assertEquals(
            listOf(
                "appops set --user 10 com.x PLAY_AUDIO allow",
                "appops get --user 10 com.x PLAY_AUDIO",
            ),
            commands,
        )
    }

    @Test
    fun aRestoreThatLeavesTheAppSilentIsReplacedWithAllow() {
        // On builds that store this op per-uid, writing `default` back over a
        // package that had nothing set leaves it reading `ignore`. `allow` is
        // the same thing said explicitly, and is what actually restores sound.
        val commands = mutableListOf<String>()
        var reads = 0
        val control = AudioControl(
            ledger = AudioMuteLedger(file = null, warn = {}),
            userId = 0,
            exec = { command ->
                commands.add(command.joinToString(" "))
                when {
                    command[1] == "get" -> {
                        reads += 1
                        if (reads == 1) 0 to "No operations." else 0 to "PLAY_AUDIO: ignore"
                    }
                    else -> 0 to ""
                }
            },
            warn = {},
        )
        assertTrue(control.mute("com.x"))
        commands.clear()
        assertTrue(control.restore("com.x"))
        assertEquals(
            listOf(
                "appops set --user 0 com.x PLAY_AUDIO default",
                "appops get --user 0 com.x PLAY_AUDIO",
                "appops set --user 0 com.x PLAY_AUDIO allow",
            ),
            commands,
        )
    }
}
