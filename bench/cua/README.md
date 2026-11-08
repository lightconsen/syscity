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
host (container-only, `runc`, darwin/arm64) they are **0.72 raw and 0.78
reachable** — six points apart, all of it one task. On an environment where more
of the suite is broken the gap would be larger, and a raw number alone would read
as model weakness.

### Run to run, this number moves more than it looks

Four full 68-variant runs at the same settings scored **0.794, 0.794, 0.735 and
0.721** — a spread of seven points with nothing changed between the first two.
Across those four runs, compared cell by cell, only **48 of the 65 cells that
completed in every run gave the same answer every time**; 17 flipped at least
once. A majority vote over the four gives 56/65 = 0.8615, *above* every single
run — a single run understates the model by roughly the amount of the flips.

So a single run cannot settle a four-point question. When judging whether a
change helped, re-run the specific variants it was meant to fix — several times
— instead of comparing two full-run totals. Two cases where that mattered:
`click-button` v5/v6 read 0/2 in one run and 7/7 on re-run, and
`right-click-menu` read 2/5 in a full run and 3/3 on three standalone re-runs.
Both were sampling noise, not regressions.

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
ignores — yet the model has solved one of them. So the picker works and the
*oracle* is what fails. `spreadsheet-cell v4` (oracle 0, model 1 in both runs)
and `video-player v2` (oracle 0, model 1 in the first) are the same story. A
rule keyed on "oracle == 0" alone would have called all three an environment
limit and quietly raised the score; an earlier version of this file did exactly
that and reported an inflated **0.927**. The figure with the guards in place is
**0.72 raw / 0.78 reachable**, and the *best* of the four runs was 0.79 — see the
variance section above.

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

Around 0.72–0.79 raw, 14–19 of 68 variants fail depending on the run. These are
the *classes* of failure, taken from replaying the trajectories and re-testing
each in a real `bench-web` container. Which variant lands in which class moves
between runs — see above — so read this as what a failure means, not as a fixed
roster:

| failure | cause | evidence |
|---|---|---|
| `drag-drop` (all 5) | **environment** | synthetic drag is rejected; `dragstart` never fires, and the target has no other route |
| `date-picker` (4–5 of 5) | model | the environment reaches it; its oracle is 0 on all five only because `solve_task` types into `input[type=date]`, which WebKitGTK ignores. Segmented `mm/dd/yyyy` sub-fields take no synthetic text at all — but a calendar-cell click does commit, and the model has passed one variant by that route |
| `video-player`'s volume variants | model, wrong layer | told to set volume to 75%, it ran `pactl set-sink-volume` in a terminal instead of moving the on-page slider; the task scores the player, not the ALSA mixer |
| `right-click-menu` misses | model stopped early, plus a race | usually one right-click then `final_answer`; the menu is there in the tree. 3/3 standalone re-runs passed at 2 steps each, so this class is mostly sampling |
| `fill-form`, `spreadsheet-cell` misses | model aimed wrong | clicks a cell or field one row off, then reports success |
| `click-button` v5/v6 | **glue bug — fixed** | see below |
| `select-dropdown` v1 | **glue bug — fixed, rewards 1 on re-run** | see below |

Apart from the three fixed glue bugs, none of these is the glue's fault. The rest
is the model stopping early, aiming at the wrong cell, or solving the task in
the wrong layer — genuine model results, and the reason the raw number is the
one to quote.

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
