# bench/ — harness glue

The only thing in this directory is the thin Python layer that lets an external
computer-use benchmark drive syscity as its agent. Nothing here scores, judges or
reimplements a task: the benchmark's own evaluator does that, and a local copy of
it would make the numbers incomparable.

```
bench/
├── osworld/
│   ├── sycity_agent.py      # OSWorld agents: replay (`bench run`) and step (`bench serve`)
│   ├── run.py               # runner over OSWorld's own lib_run_single loop
│   └── test_sycity_agent.py # unit tests; no OSWorld checkout, no VM
└── cua/
    ├── sycity_agent.py      # Cua Bench agent: one `bench run` per task, acting live
    ├── cua_bridge.py        # the guest-side HTTP desktop service (runs in the sandbox)
    ├── atspi_xml.py         # the guest's AT-SPI tree, as `parse_at_spi` reads it
    ├── pyautogui_shim.py    # pyautogui/pyperclip over xdotool and xclip
    ├── ceiling.json         # which variants this environment cannot reach, and the evidence
    ├── score.py             # raw vs. reachable, from a run's summary.json
    └── test_sycity_agent.py # unit tests; no sandbox, no Cua Bench install
```

The syscity side of the contract is `syscity bench run` / `syscity bench serve`
and their JSON — see [`docs/computer-use-benchmark.md`](../docs/computer-use-benchmark.md)
§4.3 and §4.4.

## Before a task: probe the desktop

```bash
syscity bench probe --computer-url http://<vm_ip>:5000 --execute
```

Four endpoints, a few seconds, no model time: `/screen_size`, `/screenshot`,
`/accessibility`, and (with `--execute`) a read-only `pyautogui.size()` through
`/execute`. It exits non-zero and names the endpoint that is wrong, so a broken
VM is found here rather than three model rounds into a scored task. `--json`
gives the same report to a script.

The probe never acts: the one snippet it runs reads the screen size and is not
recorded in `actions` (that field is what the agent decided, and the probe is
not the agent).

## OSWorld

Requires an OSWorld checkout with its dependencies installed (`pip install -e .`
in that checkout) and a VM provider it supports (VMware, VirtualBox, Docker+KVM,
AWS, Azure, GCP, Daytona). The checkout is imported, not vendored.

```bash
cd /path/to/OSWorld
python /path/to/syscity/bench/osworld/run.py \
  --osworld-root . \
  --provider-name docker --path-to-vm /path/to/Ubuntu.vmx \
  --split test_small \
  --syscity /path/to/syscity/target/release/syscity \
  --provider deepseek --model deepseek-v4-flash \
  --result-dir ./results/syscity
```

Each task: the harness resets the VM, syscity works the instruction against the
VM's HTTP desktop service (`http://<vm_ip>:<server_port>`), the harness executes
what it asks for through OSWorld's own `env.step`, and the task's official
evaluator scores the VM.

### Two modes

`--mode replay` (default) runs the whole task first and replays its action log
through `env.step` afterwards. One process per task, but the two loops do not
interleave: syscity decided step N+1 from its own accumulated view, so a next
step that depends on a load finishing between steps is timed by
`--sleep-after-execution` rather than by syscity's own waiting.

`--mode step` gives the harness the step itself. One `syscity bench serve`
process per task; syscity sends one snippet, blocks until the harness has
executed it and observed, then reads the screen again. That is the same ordering
OSWorld's own loop has, so use it for anything timing-sensitive.

```bash
python /path/to/syscity/bench/osworld/run.py --mode step ...   # same flags
```

Per task, under `--result-dir/<domain>/<id>/`:

| file | written by |
|---|---|
| `result.json` | syscity (the run; `actions` is what replay re-sends) |
| `serve.stderr.log` | syscity, in step mode (empty unless it complained) |
| `traj.jsonl`, `step_*.png`, `recording.mp4` | OSWorld's `run_single_example` |
| `result.txt` | OSWorld (the score) |
| `error.json` | OSWorld, when the task counted as an infrastructure error |

`summary/syscity.json` at the end reports `scored`, `passed`, `success_rate`,
`infra_errors` and `mode`. Infrastructure errors (syscity could not start, the
VM service failed, a timeout, a protocol `error` line) are **not** in the
success rate's denominator — they are listed separately, because one network
blip must not read as a worse model. Neither mode ever returns a `FAIL` action
for an infrastructure error: on an infeasible task, `FAIL` scores 1.

## Cua Bench

Requires `cua-bench` installed (`uv tool install cua-bench`) and a container
runtime — `cb` runs Linux images as containers, so no hardware virtualisation is
needed and an arm64 Mac works.

```bash
cd /path/to/syscity
PYTHONPATH=$PWD \
SYSCITY_BIN=$PWD/target/debug/syscity \
cb run dataset cua-bench-basic@1.0 \
  --agent-import-path bench.cua.sycity_agent:SyscityAgent \
  --on local --runtime runc -j 2 --max-steps 25
```

`PYTHONPATH=$PWD` is required: `cb` imports the agent by dotted path but does not
put the working directory on `sys.path`, so without it the run fails with
`ModuleNotFoundError: No module named 'bench'`.

### Known ceiling on this environment

Two numbers, and they are not interchangeable:

```bash
python3 bench/cua/score.py <run-dir>     # raw and reachable, side by side
```

**raw** — `avg_reward` over every variant — is the dataset score, and the one to
quote. **reachable** — accuracy over the variants whose whole task the *official
oracle* also fails here — is the model with the environment's own limits
removed. Which task is limited, and the per-variant oracle evidence for it,
lives in [`cua/ceiling.json`](cua/ceiling.json). Exclusion is per task and needs
all three of: oracle 0 on every variant, no variant solved by the model, and an
understood mechanism. A model success is proof the environment reached the task
and vetoes the exclusion outright — `date-picker`'s oracle fails all five
variants while the model now solves all five, so the oracle is what is broken
there, not the environment.

Before reading a low score as a model failure, run the oracle for that variant:

```bash
cb run task <dataset>/drag-drop --variant-id 0 --oracle \
  --image ghcr.io/trycua/bench-web@sha256:3b7e1d9d… \
  --on local --runtime runc
```

`drag-drop` fails its own reference solution on every variant here: it needs
synthetic HTML5 drag input, which WebKitGTK rejects under `runc` without
`/dev/uinput`. `drag-slider` is the contrast — also a drag, but reachable by
click-then-arrow-keys, and it passes. Note that oracle=0 alone is *not* proof of
a ceiling: `date-picker`'s oracle fails all five variants while the model passes
all five, and `spreadsheet-cell`/`video-player` each have an oracle-failing
variant the model passes. See [`cua/README.md`](cua/README.md).

Six full runs at the same settings scored 0.794, 0.794, 0.735, 0.721, 0.750 and
0.809, and two of those back-to-back 0.794s agreed on only 58 of 68 variants. A
seventh, after the duplicate-call fix, scored 0.838; the two after the
screen-reporting fix scored **0.910 and 0.912** (0.984 reachable both times),
differing on two of the 67 variants scored in both; the one after the
webview-value fix scored **0.926** (1.000 reachable), the first run in which
every variant outside `drag-drop` passed. Treat a single run's failure
list as a sample, not a verdict, and judge a fix by re-running the variants it
targets rather than by comparing two totals. See [`cua/README.md`](cua/README.md).

### Why there is a bridge

syscity's only remote-desktop client speaks the OSWorld VM protocol
(`/screenshot`, `/accessibility`, `/screen_size`, `/execute`). A Cua Bench
session is a `cua_sandbox.Sandbox` with its screen and input behind the SDK — a
different protocol.

Rather than teach syscity a second protocol, `cua_bridge.py` runs **inside the
sandbox** and answers those four endpoints from the guest's own X display;
`sandbox.tunnel.forward(8765)` exposes it to the host, and that URL is what
`syscity bench run --computer-url` gets. The guest image ships `xdotool`, `xclip`
and ImageMagick but not pyautogui, so `pyautogui_shim.py` implements the handful
of pyautogui/pyperclip names syscity's snippets use; `atspi_xml.py` renders the
AT-SPI tree in the shape `parse_at_spi` reads.

This is the **direct** shape, not a replay: `/execute` really runs each snippet
in the guest, so the screen syscity acts from is the screen the action changes.
Replaying recorded snippets through `session.execute_action` would be a weaker
measurement, and is what this deliberately does not do.

A syscity-side failure — binary missing, run failed, `error` set in its result —
is raised as `SyscityRunFailed`, so the harness records an infrastructure error
rather than a task the model got wrong.

### The shim has to prove its work

`xdotool` and `xclip` both fail in ways that exit 0, and every one of them makes
the model look worse than it is. `pyautogui_shim.py` therefore verifies rather
than assumes:

- **`xdotool` exits 0 on a key name it does not know** (`No such key name
  'page_up'. Ignoring it.` on stderr). The name map is explicit, and an unmapped
  name raises instead of being forwarded.
- **`xclip -loops 1` pastes into nothing in Chromium.** The browser asks the
  owner for `TARGETS` before the text, which consumes the single loop. The
  owner runs with `-loops 0` under a timeout, and `copy` reads the selection
  back before returning — so a copy that owns nothing raises, and a snippet's
  `except` fallback to `pyautogui.write` actually fires.
- **A drag has to be a sequence.** Chromium starts a native drag only after
  enough motion events; a single origin→target jump looks like a click and never
  fires `drop`. `moveTo(duration=…)` interpolates at least `DRAG_MIN_STEPS`
  points.

## Tests

```bash
python3 bench/osworld/test_sycity_agent.py
python3 bench/cua/test_sycity_agent.py
```

The Cua tests include `ScoreTest`, which pins the rule the ceiling rests on: a
variant is never excluded on the model's failure alone, and never excluded at
all once the model has solved it.

They use scripted stand-ins for the syscity binary: the command line, the replay
order, the step protocol's turn order (`task` → `action`/`result`... → `done`),
and the "infrastructure error is not a wrong answer" boundary are checked without
a VM, a model or a Rust build. The Cua tests additionally cover the bridge
handoff — every bridge file uploaded, the tunnel port, a bridge that never comes
up reporting its own log — and that the trajectory reaches the tracer through
`record()`, which is the interface Cua Bench's `Tracing` actually has.

The Cua tests import `cua_bench` when it is installed and stub the interface they
need when it is not, so either interpreter runs them.
