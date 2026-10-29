# bench/ — harness glue

The only thing in this directory is the thin Python layer that lets an external
computer-use benchmark drive syscity as its agent. Nothing here scores, judges or
reimplements a task: the benchmark's own evaluator does that, and a local copy of
it would make the numbers incomparable.

```
bench/
└── osworld/
    ├── sycity_agent.py      # OSWorld agent: one `syscity bench run`, replayed
    ├── run.py               # runner over OSWorld's own lib_run_single loop
    └── test_sycity_agent.py # unit tests; no OSWorld checkout, no VM
```

The syscity side of the contract is `syscity bench run` and its result file —
see [`docs/computer-use-benchmark.md`](../docs/computer-use-benchmark.md) §4.3.

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

Each task: the harness resets the VM, syscity runs the instruction once against
the VM's HTTP desktop service (`http://<vm_ip>:<server_port>`), and then the
snippets it sent are replayed through OSWorld's own `env.step`, after which the
task's official evaluator scores the VM.

Per task, under `--result-dir/<domain>/<id>/`:

| file | written by |
|---|---|
| `result.json` | syscity (the run, including `actions` for the replay) |
| `traj.jsonl`, `step_*.png`, `recording.mp4` | OSWorld's `run_single_example` |
| `result.txt` | OSWorld (the score) |
| `error.json` | OSWorld, when the task counted as an infrastructure error |

`summary/syscity.json` at the end reports `scored`, `passed`, `success_rate`
and `infra_errors`. Infrastructure errors (syscity could not start, the VM
service failed, a timeout) are **not** in the success rate's denominator — they
are listed separately, because one network blip must not read as a worse model.

Known limitation: replay does not interleave the two loops. syscity decided each
step from its own accumulated view of the screen, so a next step that depends on
a load finishing between steps is timed by `--sleep-after-execution` rather than
by syscity's own waiting. The step protocol (M3 in the plan) removes this.

## Tests

```bash
python3 bench/osworld/test_sycity_agent.py
```

They use a scripted stand-in for the syscity binary: the command line, the
replay order, and the "infrastructure error is not a wrong answer" boundary are
checked without a VM, a model or a Rust build.
