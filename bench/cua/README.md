# bench/cua — the Cua Bench glue

The thin layer that lets `cua-bench` drive syscity as its agent, plus the one
thing a benchmark run needs and a benchmark run cannot tell you by itself: **how
much of the environment it was scored in actually works.**

```
bench/cua/
├── sycity_agent.py        # the agent: one `bench run` per task, acting live
├── cua_bridge.py          # the guest-side HTTP desktop service
├── atspi_xml.py           # the guest's AT-SPI tree, as `parse_at_spi` reads it
├── pyautogui_shim.py      # pyautogui/pyperclip over xdotool and xclip
├── ceiling.json           # the environment ceiling, with its evidence
├── score.py               # raw vs. reachable, from a run's summary.json
└── test_sycity_agent.py   # unit tests; no sandbox, no Cua Bench install
```

## Running it

```bash
PYTHONPATH=$PWD SYSCITY_BIN=$PWD/target/debug/syscity \
cb run dataset cua-bench-basic@1.0 \
  --agent-import-path bench.cua.sycity_agent:SyscityAgent \
  --on local --runtime runc -j 2 --max-steps 25
```

`PYTHONPATH=$PWD` is not optional — `cb` imports the agent by dotted path but
does not put the working directory on `sys.path`.

## The two scores

```bash
python3 bench/cua/score.py <run-dir>          # human
python3 bench/cua/score.py <run-dir> --json   # machine
```

| | what it is | what it is for |
|---|---|---|
| **raw** | `avg_reward` over every variant, straight from `summary.json` | the dataset score — **quote this one** |
| **reachable** | accuracy over the variants this environment can actually solve | the model, with the environment's own limits removed |

They are not interchangeable and neither may be presented as the other. On this
host (container-only, `runc`, darwin/arm64) the latest full run is **0.926 raw,
1.000 reachable** (`63/68` and `63/63`); the two before it were 0.910 and 0.912
raw (0.984 reachable both times). The gap between raw and reachable is entirely
`drag-drop`, the one task this environment cannot reach. On an environment where
more of the suite is broken the gap would be larger, and a raw number alone would
read as model weakness.

### Run to run, this number moves more than it looks

Nine full 68-variant runs scored **0.794, 0.794, 0.735, 0.721, 0.750, 0.809,
0.838, 0.910 and 0.912**, and a tenth **0.926** — a spread of twenty points, and
not one population.
The first four differ from each other only by sampling, because nothing changed
between them. Across those four, compared cell by cell, only **48 of the 65
cells that completed in every run gave the same answer every time**; 17 flipped
at least once. A majority vote over the four gives 56/65 = 0.8615, *above* any of
them — a single run understates the model by roughly the amount of the flips.

The 0.750 and 0.809 pair is not pure noise: it was measured across a settle fix
(below), and the tasks that fix was aimed at moved with it — `select-dropdown`
3/5 → 5/5 and `right-click-menu` 2/5 → 4–5/5. That is the honest reading of the
two runs as a pair, not a claim that the fix is worth seven points; re-running
the specific variants is the only way to tell the two apart, which is what the
next paragraph says.

The last four are a different regime. The 0.838 run was the first after the
duplicate-call guard, and it is where `date-picker` went **0/5 → 5/5** — a fix to
the agent loop, not to the bench glue (see "A repeated action is not a repeated
call", below). The 0.910 and 0.912 runs came after the screen-reporting fix. On
the 67 variants scored in both, they differ on two cells and cancel out —
`fill-form v0` passed in the first and not the second, `typing-input v4` the
other way round — so the two runs land on the same **61/67**; the second only
looks higher because the first's `right-click-menu v2` was an infrastructure
error (the completion stream dropped) and so was never scored. Against 0.838,
five cells moved and all five moved up: four `toggle-switch` variants coming
back, plus `typing-input v2`. The toggle half is the unstyled-page sample
described below — an environment recovery, not a gain.

The 0.926 run is the one after the webview-value fix, and it is the cleanest
reading of the model so far: every variant outside `drag-drop` passed, so raw
and reachable coincide at 63/63. Against the 0.912 run it differs in exactly one
cell — `fill-form v0`, back up — and against the 0.910 run in exactly one scored
cell: `typing-input v4`, which is the one the fix was aimed at. (The other cell
that separates it from 0.910 is `right-click-menu v2`, which was an
infrastructure error in that run and so was never scored.) That is not a claim
the fix is worth 1.4 points. `fill-form v0` alternated 0, 1, 0, 1 across the
last four runs and `typing-input v4` had already passed in the 0.912 run, so
both were within the range each cell showed before the fix. A three-round
controlled A/B against a pre-fix copy of the glue (`typing-input v4` 2/3 → 3/3,
`fill-form v0` 3/3 → 3/3, p = 1.0) puts the same bound on it. What the fix *does*
change is that a correct `type` now reports as verified instead of falling
through to a pixel diff that cannot see a 43-character edit, which is what
stopped the model retyping on top of text that already landed — the same A/B
measures 0/6 → 5/6 turns with the value in the tree, p = 0.015 (see "A webview
text field's value was never in the tree", below). The honest statement is that
the score has been **0.91–0.93 across three consecutive runs**, with a reachable
ceiling that is now 1.000 because the only task this environment excludes is
`drag-drop`.

So a single run cannot settle a four-point question. When judging whether a
change helped, re-run the specific variants it was meant to fix — several times
— instead of comparing two full-run totals. Two cases where that mattered:
`click-button` v5/v6 read 0/2 in one run and 7/7 on re-run, and
`right-click-menu` read 2/5 in a full run and 3/3 on three standalone re-runs.
Both were sampling noise, not regressions.

### The settle window, and why "0.0% changed" was sometimes a lie

A step used to settle with `initial_delay_ms: 150`, `poll_interval_ms: 120`,
`max_rounds: 4` — a window of 150 + 3 × 120 = **510 ms**. For an action whose
repaint lands after that, two problems compounded:

1. **Two polls compared against each other, not against the baseline.** The
   loop stopped at the first pair of identical frames — which is equally what
   "the screen has not started changing yet" looks like.
2. **The adapter's screenshot cache answered both polls with one frame.** A
   capture is reused for 250 ms, longer than the 120 ms poll spacing, so the
   first poll returned the *same stored frame* the baseline was stored from.
   The loop's first comparison was therefore trivially stable.

Measured on the real image, a right-click that opens a context menu repaints
**205 ms** after the click on a good run and **1.4 s** on a slow one — both
outside or at the edge of the old window. The agent was told "0.0% changed in
pixels", concluded its right-click had done nothing, and answered honestly that
the menu never opened. It had.

The fix has two halves, both in `src/computer/session.rs` and
`src/computer/remote/osworld.rs`:

* `SettleConfig::min_window_ms` (default 1200 ms): until *something* has
  changed, keep comparing **against the baseline** and keep polling. Once a
  change is seen, consecutive frames decide "settled" as before. An action that
  visibly works still settles immediately; only the ambiguous case waits.
* `ComputerAdapter::screenshot_fresh`, which the settle loop now calls, and
  which `OsworldDesktop` implements to bypass its reuse window. The cached
  `screenshot` is still what the model's own reads use.

Two regression tests pin both directions: a repaint landing after the first
polls is seen and verified, and a screen that never repaints is still reported
unchanged (the window ends the search; it does not promise a change).

## The environment ceiling

`ceiling.json` declares which tasks this environment cannot reach, and **why the
claim is trustworthy**: for each one, the dataset's own reference solution — the
official oracle — also fails, on every variant.

```bash
cb run dataset cua-bench-basic@1.0 --oracle --on local --runtime runc -j 2 \
  --image ghcr.io/trycua/bench-web@sha256:3b7e1d9d… \
  --output-dir /tmp/cua-oracle
```

The oracle is good evidence for a ceiling — if the dataset cannot solve its own
task here, no agent should be blamed for failing it — but it is not sufficient
on its own, and `score.py` enforces the limits. A task is excluded only when all
three hold:

1. **The oracle scored 0 on every variant** the run contains. A `null` means
   *not measured*, and blocks the exclusion; it never counts as zero.
2. **The model solved no variant of it.** A solved variant is proof the
   environment reached the task, so it vetoes the exclusion outright.
3. **The failure mechanism is understood** and written down in the file.

That second rule is not hypothetical. `date-picker`'s oracle scores **0 on all
five variants** — its `solve_task` types into `input[type=date]`, which WebKitGTK
ignores — yet the model now solves all five of them. So the picker works and the
*oracle* is what fails. `spreadsheet-cell v4` (oracle 0, model 1 in both runs)
and `video-player v2` (oracle 0, model 1 in the first) are the same story. A
rule keyed on "oracle == 0" alone would have called all three an environment
limit and quietly raised the score; an earlier version of this file did exactly
that and reported an inflated **0.927**. The figure with the guards in place is
**0.926 raw / 1.000 reachable** on the latest full run — close to that inflated
number by coincidence, not by construction: raw is 0.926 because five
`drag-drop` variants failed and everything else passed, and reachable is 1.000
because `drag-drop` is the only exclusion and the model is 63/63 on the rest.
The four runs before the settle fix ranged 0.721–0.794 — see the variance
section above.

### Two runs at the same score still disagreed on ten cells

Two full 68-variant runs with the same shim and model, back to back, both scored
**0.794** — but they agreed on only **58 of 68** variants. Ten flipped, five each
way:

```
date-picker v4      1 -> 0        right-click-menu v3  0 -> 1
right-click-menu v4 1 -> 0        select-dropdown   v1 0 -> 1
select-dropdown v2  1 -> 0        spreadsheet-cell  v2 0 -> 1
video-player v2     1 -> 0        video-player      v3 0 -> 1
video-player v5     1 -> 0        fill-form         v2 0 -> 1
```

Re-running six of those flips on its own settled five of them the way run 2 did
(`select-dropdown` v2 came back 1, matching run 1 instead). Two cells were wrong
for a *known* reason and are now fixed — `select-dropdown` v1 and `fill-form`
v2 — but the rest is sampling noise: a 25-step budget and a model that stops
early. The score is the same in both runs because the flips cancelled out; that
is a coincidence, not a guarantee.

This is why the ceiling is declared per **task**, never per variant. A per-cell
rule would read run 1 as evidence that `date-picker` is limited and run 2 as
evidence that `right-click-menu` is, and neither claim survives the other run.
And whole-task, all-or-nothing exclusion is deliberate for the same reason:
excluding variant by variant would let a run drop precisely the variants the
model failed, which is cherry-picking rather than a ceiling.

### What is actually limited here

`drag-drop`, all five variants: an HTML5 drag needs synthetic drag input, and
WebKitGTK rejects it under `runc` without `/dev/uinput`. Page-side event
listeners confirm the diagnosis — `mousedown`, `mousemove` and `mouseup` all
arrive at the page, `dragstart` never fires, even at 30 dense steps. There is no
other input route to the target: it only reacts to a `drop` event.

`drag-slider` is *not* limited, and is the useful contrast: it is a drag too, but
the model reaches it with click-then-arrow-keys, and it scores 1 on all five
variants.

### Adding to the ceiling

Run the oracle for the whole task, then record it per variant:

```bash
cb run task <dataset>/<task> --oracle \
  --image ghcr.io/trycua/bench-web@sha256:3b7e1d9d… \
  --on local --runtime runc
```

Add the task to `unreachable_tasks` with every variant's measured
`oracle_reward`, and only if the model has never scored 1 on any of them. If
even one variant is reachable — by the oracle or by the model — the task stays
in the denominator, and `score.py` will say so rather than apply the exclusion.

## Where the failures come from

Across the nine runs 5–19 of 68 variants fail depending on the run. These are
the *classes* of failure, taken from replaying the trajectories and re-testing
each in a real `bench-web` container. Which variant lands in which class moves
between runs — see above — so read this as what a failure means, not as a fixed
roster:

| failure | cause | evidence |
|---|---|---|
| `drag-drop` (all 5) | **environment** | synthetic drag is rejected; `dragstart` never fires, and the target has no other route |
| `spreadsheet-cell` misses | model aimed wrong | clicks a cell one row off, then reports success |
| `video-player`'s volume variants | model, wrong layer | told to set volume to 75%, it ran `pactl set-sink-volume` in a terminal instead of moving the on-page slider; the task scores the player, not the ALSA mixer |
| `click-button` v5/v6 | **glue bug — fixed** | see below |
| `select-dropdown` v1 | **glue bug — fixed, rewards 1 on re-run** | see below |
| `date-picker`, every variant | **agent-loop bug — fixed, 5/5** | see below |
| `toggle-switch` (4 of 5) | **environment, not the model** | the page rendered its content but no styling — see below |
| `typing-input v4`, `fill-form v0` misses | **glue bug — fixed, both reward 1 on re-run** | see below |

None of the fixed glue bugs, nor the agent-loop bug, is the model's fault or the
environment's. The rest is the model stopping early, aiming at the wrong
cell, or solving the task in the wrong layer — genuine model results, and the
reason the raw number is the one to quote.

### A webview text field's value was never in the tree

This is the one class that took a measurement to settle rather than a
trajectory read. Both `typing-input v4` and `fill-form v0` reported, from
inside the run, that typing had not landed: `typing-input`'s agent said the
field "drops the first keystroke of every `type` action", and `fill-form`'s said
the Age field double-registered (`25252252525`, then `225`, finally `25`). Both
were `MAX_STEPS_EXCEEDED` with reward 0, and both sat downstream of `type`
moving to `pyautogui.write`, which made it a plausible regression.

It is not. Reproduced in a `bench-web` container with the task's own pywebview
window, `pyautogui.write` delivered all 43 characters on every one of six
attempts, and `pyautogui_shim` never dropped a keystroke. What went wrong was
the read on the way back:

* `atspi_xml.py::_value` asked every node only `queryValue()`. WebKitGTK's
  `entry` — what every `input` and `textarea` in a task window exposes — raises
  `NotImplementedError` there, while `queryText()` answers with the real
  content. So a webview text field published **no value at all**.
* That silently disabled the tree half of typing verification.
  `typed_value_hint` needs an editable field whose value moved to compare
  against the text; with every field reading `None`, it returned `None` and
  `verify` fell through to the pixel diff.
* The pixel diff cannot see typing into a short field. Measured on the real
  frames: 43 characters landing in the 352x42 input changed 0.0% of the screen
  at the verifier's 32x32-block threshold of 30, and clearing it with `Delete`
  changed 0.9% — the one step that verified in the failed run was the one that
  emptied the field.

So the typing landed, the screen did not report it, and the model was told
"no visible change detected". It then retyped on top of text that was already
there: in a probe of the same sequence, type-then-retype produced exactly the
43+43 characters the agent saw, and `ctrl+a` did select everything, but a
`Home` that reports no change invites a `Delete` that removes one character.
The `#` in `#The quick…` was the model's own diagnostic prefix, typed on
purpose.

The fix is one fallback in `atspi_xml.py`: roles `entry` and `text field` ask
`queryText()` when `queryValue()` is not implemented. Two roles are excluded
on purpose — static `text`, because the reader maps it to `text_field` and every
paragraph would become an editable field — and `password text`, because there
`queryText` returns the plaintext, which would write a credential into the turn
log and the trace. A date input reports no value either before or after the
change, so the `date-picker` path is untouched.

`fill-form v0`'s Age field had the same cause: `25` landed first time and the
"double registration" was three retypes of text that was already right.

A single re-run per arm already showed reward 1, and the `typing-input` one
showed the value in the tree, but that is one observation either way. The
controlled version is twelve runs: three rounds, each round running both
variants against the fixed tree and then both against a pre-fix copy of
`bench/cua/` (every file identical except `atspi_xml.py` at the commit before
the fix), interleaved so any drift in the model, the image or the host hits both
arms.

| | `typing-input v4` | `fill-form v0` |
|---|---|---|
| pre-fix | 1, 1, **0** (74s, 57s, 184s) | 1, 1, 1 (120s, 148s, 128s) |
| fixed | 1, 1, 1 (35s, 80s, 31s) | 1, 1, 1 (103s, 166s, 99s) |

**On reward this is nothing.** 5/6 against 6/6, Fisher exact p = 1.0. Both cells
were known to flip before the fix and three samples per arm cannot move a
two-in-three cell. Do not quote this table as a score gain.

**The mechanism is where the measurement lands**, and there the split is clean:

| | pre-fix | fixed | p |
|---|---|---|---|
| turns whose tree carried the typed value (`typing-input`) | 0/3 | 3/3 | 0.10 |
| turns whose tree carried the typed value (`fill-form`) | 0/3 | 2/3 | 0.40 |
| both tasks together | **0/6** | **5/6** | **0.015** |
| `type` steps whose effect verified as `confirmed` | 9/22 (41%) | 14/17 (82%) | 0.020 |

The value shows up in both read paths — the `ui_tree` section of a
`screen_state` call as well as a `computer` `read_ui_tree` — which is why
`fill-form` carries it in two turns out of three and not all three: one turn
never asked for the tree at all.

The retype loop reproduces on one side only. The pre-fix `typing-input v4` that
failed took 184s and 18 calls: `type` reported `suspected_noop`, then `ctrl+a`,
`ctrl+c`, `clipboard_get`, a click on a stale element id, `shift+Home`,
`BackSpace`, another `type`, `ctrl+c` again. The three fixed runs took **3, 6 and
2** calls (pre-fix: 8, 5, 18), mean 105s → 49s. That is the same failure the
0.910 run showed, now with a control beside it.

Run directories: `ab-data/prefix-r{1,2,3}` and `ab-data/fix-r{1,2,3}` under the
job tmp tree; each holds one `result.json` per variant.

### A repeated action is not a repeated call

Every desktop variant of `date-picker` failed in one run and every one passed in
the next. The cause was in `src/agent/engine`, not `bench/cua`: a guard written
to stop a model looping on a failing `grep` compared `(tool name, arguments)`
against everything already executed in the turn and **skipped any byte-identical
repeat**.

On a shell that guard is right — the same `grep` over the same file cannot say
anything new. On a desktop it is wrong: a second click on a calendar's "previous
month" arrow is a *second* month, and a second `screen_state` is a *different*
screen. The turn ended with the guard's own note ("NOT_EXECUTED") and the model
concluded it had failed.

`ToolCapabilities::deduplicate_identical_calls` now decides this per tool. It
defaults to `true` — the loop protection stays for pure reads — and `computer`
and `screen_state` set it to `false` with the reason recorded in
`src/tools/{computer,screen_state}.rs`. Two engine tests pin both directions: an
identical call to a world-changing tool runs every time, and an identical call
to a pure tool still runs once.

`date-picker` scored **5/5** in the next full run. One wrinkle in reading that
run: three variants (`date-picker v4`, `video-player v5` and `v6`) have
`status: "failed"` with `error: MAX_STEPS_EXCEEDED` yet `reward: 1.0` — they
finished the task and ran out of steps on the way out. `status` counts a run as
completed only when the agent stops cleanly; `avg_reward` counts the reward. The
score is the evaluator's, so 57/68 stands. A later run scored
`spreadsheet-cell v0` the same way — failed on steps, reward 1.

### The toggle that was not there

`toggle-switch` scored 5/5 in six consecutive full runs, then 1/5 in one. The
cause was not the agent: in that run **the page rendered without its
stylesheet**. The task's HTML loads Tailwind from `cdn.jsdelivr.net` inside the
sandbox, and when that fetch fails the page still renders — as unstyled text.
The evidence is in the accessibility tree, which reports the switch's real
layout box:

```
styled   button "Toggle notifications" at (785,282) 44x24
unstyled button "Toggle notifications" at (428,353) 16x10
```

An unstyled switch is a 16×10 target with no visible on/off state, sitting under
a text label — the model clicked the label (`focus: false → true`), pressed
space, saw no change, and reported that the toggle did not respond. That run's
`click-button` buttons were a single row of grey text where the styled page draws
a blue two-column grid, and its `select-dropdown` was plain text where the styled
page draws a card — so the failure is a property of the run, not of the task.

A same-binary re-run of all five variants scored **5/5, one click each**, and the
tree shows the styled box. Two full runs since then have also scored it 5/5, so
treat a `toggle-switch` miss as an environment sample until the styled geometry
is confirmed: the 16×10 box is the tell.

### A click at a coordinate the tree published, but the window had cropped

`click-button`'s window is 256×256 and its button grid grows to eight buttons, so
the target can land on a fourth row **below the fold**. AT-SPI keeps reporting
that row's layout box — flagged `visible` but not `showing` — and a click at
those coordinates lands on the bare desktop behind the window. Fixed twice over,
because both halves were wrong:

* `atspi_xml.py` now withholds the box for a node that is `visible` without
  `showing`. The element still appears in the tree (it exists), it just has no
  clickable point. Measured in a container: of eight buttons in the 256-px
  window, six keep their real boxes and the two on the cropped row lose theirs.
* `parse_at_spi` no longer reads AT-SPI's `active` as keyboard focus. `active`
  marks the *window* that has the pointer's attention; a frame carries it while
  nothing inside is focused. Reading it as focus meant that clicking **outside**
  the window (the window drops `active`) verified as a working action — which is
  exactly how a click at y=620 in a window ending at y=557 came back "clicked
  Close" while the real button was untouched. The native x11 reader had always
  used `STATE_FOCUSED` alone; the two now agree, and `tree_diff` additionally
  only counts a focus transition on a role that can hold keyboard focus.

After the fix `click-button` scored 7/7 in the next full run.

### A timeout is not a refusal

`fill-form` v2's own answer was *"Scrolling via wheel/drag is refused by the
remote endpoint"*. The endpoint refused nothing. syscity encodes a scroll
`amount` as `amount * 100` wheel clicks (`osworld.rs:758`), and `xdotool click
--repeat` sleeps **100 ms between clicks by default**. So `scroll(-500)` took
51 s, blew the remote client's 30 s request timeout, and arrived at the model as
a failure it had to explain. Measured in the container with CDP reading
`window.scrollY`:

```
xdotool click --repeat 500 5            51.07s   scrollY 0 -> 5369
xdotool click --delay 0 --repeat 500 5   0.27s   scrollY 0 -> 5369
```

Identical distance — Chromium coalesces the dense wheel events. `scroll` and
`hscroll` now pass `--delay 0`, and `WheelTest` pins it. This is the case that
justifies the ceiling's third rule: before calling a failure environmental,
check that the mechanism is understood rather than inferred from whatever the
model said while giving up.

It did **not** turn `fill-form` into a reliable pass on its own. Three standalone
re-runs of v2 right after the fix still scored 0 (one filled and submitted the
form but with wrong values); the next full run scored it 1 and took the task to
3/3. The page is reachable now, and the remaining misses are the model's.

## The shim has to prove its work

`xdotool` and `xclip` both fail in ways that exit 0, and every one of them makes
the model look worse than it is. `pyautogui_shim.py` verifies rather than
assumes — `xdotool` prints `No such key name` and exits 0 on a key it does not
know; `xclip -loops 1` pastes into nothing because a browser asks for `TARGETS`
first; a drag needs a real sequence of motion events, not an origin→target jump.
Each is covered by a test.

`atspi_xml.py` has the same problem in a quieter form: a **closed `<select>`
reports every option at the combo box's own box**, so all four options of a
dropdown arrive at identical coordinates. A model told to pick "Banana" clicks
there, hits the combo box, and closes the menu — which is exactly how
`select-dropdown` v1 was lost. The real rows only exist while the popup is open,
in a separate top-level `window` → `tree table` → `table cell` subtree; the
stale copies under the combo box are still in the tree and come first.

`real_option_boxes()` remaps only the options whose reported box is
*provably* the collapsed fiction, and is deliberately conservative: a label is
remapped only when every copy under the combo box shares the combo box's own
box, a closed popup (a 0×0 `menu`) yields nothing so the tree is unchanged, and
a label appearing twice in a popup is skipped rather than guessed. Re-running
`select-dropdown` v1 against the fixed shim scores **1**, where it previously
scored 0 with the model correctly diagnosing "the accessibility coords for menu
items are wrong" and then giving up.
