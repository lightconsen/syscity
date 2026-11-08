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
host (container-only, `runc`, darwin/arm64) they are **0.794 raw and 0.857
reachable** — six points apart, all of it one task. On an environment where more
of the suite is broken the gap would be larger, and a raw number alone would read
as model weakness.

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
ignores — yet the model solved v4 by opening the picker and using its spin
buttons. So the picker works and the *oracle* is what fails. `spreadsheet-cell
v4` and `video-player v2` are the same story. A rule keyed on "oracle == 0"
alone would have called all three an environment limit and quietly raised the
score; an earlier version of this file did exactly that and reported an inflated
**0.927**. The figure with the guards in place is **0.794 raw / 0.857 reachable**.

Whole-task, all-or-nothing exclusion is deliberate too: excluding variant by
variant would let a run drop precisely the variants the model failed, which is
cherry-picking rather than a ceiling.

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

## Where the 14 failures come from

At 0.794 raw, 14 of 68 variants fail. Every one was replayed from its trajectory
and re-tested in a real `bench-web` container — the split matters, because only
five of them are the environment's fault:

| variant | cause | evidence |
|---|---|---|
| `drag-drop` v0–v4 | **environment** | synthetic drag is rejected; `dragstart` never fires, and the target has no other route |
| `date-picker` v0–v3 | model | the model solves v4 of the *same task*, so the environment reaches it. Its oracle is 0 on all five only because `solve_task` types into `input[type=date]`, which WebKitGTK ignores |
| `fill-form` v2 | model (a glue bug was found on the way, and fixed) | see below |
| `select-dropdown` v1 | **glue bug — fixed and re-verified at reward 1** | see below |
| `right-click-menu` v3 | model gave up | one right-click, then `final_answer`; the page's AX is correct (`menu item 'Delete' (406,256,55,27)`) |
| `video-player` v3 | model gave up | one slider click, read "26%", said "nudging it up", stopped; a single synthetic click puts `input[type=range]` at 30% exactly |
| `spreadsheet-cell` v2 | model aimed wrong | target C3 is (668,332); it clicked (545,369), which is B4, then typed the formula there and claimed C3 |

Only the two glue bug rows changed the accounting. `select-dropdown` v1 goes from
0 to 1 once `real_option_boxes()` is in place. `fill-form` v2 does **not**: after
the scroll fix it was re-run three times and still scored 0 (one run did fill the
form and submit, but the field values were wrong). It stays a model failure and
stays in the denominator — which is the point of the rule below, and the reason
a "glue bug" does not get to mean "excluded".

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
