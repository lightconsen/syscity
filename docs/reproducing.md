# Reproducing the published numbers

Every score in the README comes from the benchmark's own evaluator. This page is
the shortest path from a clean checkout to the same number, and it names the
places where a run silently measures something other than what you think.

If a number here disagrees with the README, the README is stale — say so.

## 0. What each number actually measures

Read this before comparing anything to a published leaderboard row.

| Benchmark | The claim | What the score means |
|---|---|---|
| BFCL `raw` mode | 80.12% (673/840) | `deepseek-v4-flash` answering BFCL's requests through syscity's OpenAI-compatible endpoint. syscity supplies the wire format; the tool-calling ability is the model's. |
| GAIA validation L1 | 90.6% (144/159 trials) | syscity's own agent loop — its system prompt, tool descriptions, iteration budget and model — on 53 public validation questions, 3 trials each. |

Neither is submittable to its leaderboard as-is, and neither is comparable to a
published row:

- **BFCL** takes a pull request that adds a model handler (`CONTRIBUTING.md` in
  `ShishirPatil/gorilla`), and only for a publicly reachable endpoint. A PR here
  would register *DeepSeek* behind our endpoint, which already has its own row.
- **GAIA** closed its validation leaderboard; only a full test-split run
  (Levels 1–3, 301 questions) is submittable. We have not run that.

Treat both as regression baselines: they answer "did my change make this
worse?", not "where do we rank?".

## 1. BFCL

The adapter is `evals/benchmarks/bfcl_syscity.py`. It injects a model entry into
`bfcl_eval`'s `MODEL_CONFIG_MAPPING` and then hands off to the unmodified
upstream CLI — so the accuracy report you get is the official evaluator's.

```bash
# once, outside the repo
uv venv ~/.cache/syscity-bench/bfcl
uv pip install --python ~/.cache/syscity-bench/bfcl/bin/python bfcl-eval
# bfcl-eval does not declare this optional dependency of qwen_agent:
uv pip install --python ~/.cache/syscity-bench/bfcl/bin/python soundfile
```

The gateway the adapter talks to must run with `mode: raw` semantics — BFCL
sends `tools` and no `mode` field, and the gateway's default for that shape is
`raw` (`OpenAiChatRequest::effective_mode`). If that default ever changes, every
call turn comes back empty instead of scoring.

```bash
# gateway on :18081, auth off (no security.auth_required, no shared_token)
export BFCL_PROJECT_ROOT=~/.cache/syscity-bench/run   # keep results out of the repo
export OPENAI_BASE_URL=http://127.0.0.1:18081/v1
export OPENAI_API_KEY=local
export BFCL_SYSCITY_MODEL=syscity-agent              # sentinel → gateway default

python evals/benchmarks/bfcl_syscity.py generate --model syscity \
    --test-category simple_python,irrelevance,multi_turn_base
python evals/benchmarks/bfcl_syscity.py evaluate --model syscity \
    --test-category simple_python,irrelevance,multi_turn_base
```

A full run is 840 cases and about 1.5 hours. Two things decide whether it scores
at all: the `raw` default above, and `underscore_to_dot=True` in the adapter
(BFCL sends `math.factorial` as `math_factorial`; without the flag the checker
marks correct calls wrong).

## 2. GAIA validation Level 1

53 gated questions, 3 trials each = 159. Judged by GAIA's own
`question_scorer`, vendored verbatim in `evals/importers/gaia_scorer.py`.

```bash
# once per machine: Python has no stdlib parquet reader
python3 -m venv ~/.cache/syscity-bench/gaia/venv
~/.cache/syscity-bench/gaia/venv/bin/pip install pyarrow pyyaml

# token: the account that accepted the terms at
# https://huggingface.co/datasets/gaia-benchmark/GAIA
HF_TOKEN=... ~/.cache/syscity-bench/gaia/venv/bin/python evals/importers/gaia.py

syscity eval run gaia_l1 --full --trials 3
```

The importer writes `evals/capability/gaia_l1.yaml` and `gaia_l1_answers.json`,
both gitignored (the GAIA terms forbid redistribution and attachment paths are
machine-specific). `evals/suites/gaia_l1.yaml` is the checked-in manifest.

Where a run fails for harness reasons rather than model reasons:

- **Network.** `shell` runs with the production allowlist environment, which
  clears the proxy vars, while `web_fetch`/`browser` inherit the process env. On
  a host that needs a proxy for some sites, a `curl`-based route fails where the
  same page via `web_fetch` succeeds. The ledger records four `curl` exit-28
  trials from this.
- **Images.** The `image` tool returns metadata; pixels reach the model through
  the OCR preset. A question whose answer needs an image the OCR cannot read is
  structurally out of reach, not a sampling accident.

Report both the trial rate (159 denominator) and the task rate (53 denominator):
the three trials of a question share a prompt and a model, so they are not
independent samples. The last run was 90.6% trial / 81.1% all-three-pass.

## 3. Cua Bench

Computer-use scoring, and the only one runnable on this machine — container
tasks need no hardware virtualization.

```bash
uv tool install 'cua-bench[browser]'
uv tool run --from 'cua-bench[browser]' playwright install chromium

cd /path/to/syscity
PYTHONPATH=$PWD \
SYSCITY_BIN=$PWD/target/debug/syscity \
cb run dataset cua-bench-basic@1.0 \
  --agent-import-path bench.cua.sycity_agent:SyscityAgent \
  --on local --runtime runc -j 2 --max-steps 25
```

`PYTHONPATH=$PWD` is mandatory: `cb` imports the agent by dotted path but does
not put the working directory on `sys.path` (`ModuleNotFoundError: bench`).
**Do not export `http_proxy`/`https_proxy` for `cb`** — the sandbox network is
not the host network, and a proxied run hangs.

Results land in `~/.local/share/cua-bench/runs/<run id>/<task>_v<n>/`. Score a
run directory with:

```bash
python3 bench/cua/score.py <run-dir>     # raw and reachable, side by side
```

Quote **raw** — `avg_reward` over every variant. **reachable** excludes the
variants whose whole task the official oracle also fails on this host, and is
only meaningful alongside [`cua/ceiling.json`](../bench/cua/ceiling.json), which
records the per-variant oracle evidence. Published: 0.926 raw / 1.000 reachable,
over a nine-run spread of 0.72–0.93 — a single run says little.

## 4. Where a number is recorded

`evals/ledger.md` holds one row per run — date, commit, trials, rate, and the
cost in wall-clock and tokens. When quoting a score, quote its commit; a number
without one cannot be checked out and re-run.
