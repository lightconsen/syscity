# bench/ — harness glue

The only thing in this directory is the thin Python layer that lets an external
computer-use benchmark drive syscity as its agent. Nothing here scores, judges or
reimplements a task: the benchmark's own evaluator does that, and a local copy of
it would make the numbers incomparable.

```
bench/
└── osworld/
    ├── sycity_agent.py      # OSWorld agents: replay (`bench run`) and step (`bench serve`)
    ├── run.py               # runner over OSWorld's own lib_run_single loop
    └── test_sycity_agent.py # unit tests; no OSWorld checkout, no VM
```

The syscity side of the contract is `syscity bench run` / `syscity bench serve`
and their JSON — see [`docs/computer-use-benchmark.md`](../docs/computer-use-benchmark.md)
§4.3 and §4.4.

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

## Tests

```bash
python3 bench/osworld/test_sycity_agent.py
```

They use scripted stand-ins for the syscity binary: the command line, the replay
order, the step protocol's turn order (`task` → `action`/`result`... → `done`),
and the "infrastructure error is not a wrong answer" boundary are checked without
a VM, a model or a Rust build.
