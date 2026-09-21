# Syscity Agent Harness — Design Document

> Status: reflects the implemented codebase as of 2026-08-28.
> This document describes the harness **as built**: the mechanisms, the design
> intent behind each, and the points that still need consideration. It is a
> design document, not a roadmap checklist.
> Related docs: `docs/arch.md` (system architecture), `docs/eval-status.md`
> (evaluation methodology), `docs/reflection.md` (reflection engine).

---

## 1. Purpose

An agent harness is not a collection of parts — it is a **closed-loop control
system** that connects the model to the outside world. The model reasons; the
harness makes sure the system can *act, observe, measure, intervene, and
recover*.

The skeleton (static parts) becomes a harness (closed loop) when the final
loop is added: **the evaluation loop**. Every output is measured, and the
measurement feeds back into improving the system.

```
  Skeleton (static parts)                Harness (closed loop)
  ───────────────────────                ─────────────────────
  engine · tools · routing · memory      + tool contracts (verifiable, misuse blocked)
  · session · context                    + observation loop (every step replayable)
                                         + state recovery (crash-resumable)
                                         + evaluation loop (every output scored,
                                           comparable, regressable)
                                         + optimization loop (badcase → fix → gate)
```

Design goals that shape every decision in this document:

1. **Regressions matter more than absolute scores** — "is this week worse than
   last week" is more actionable than "the score is 85 now".
2. **Judges are imprecise instruments** — calibrate before use; never trust a
   single raw LLM score.
3. **Process quality ≠ result quality** — a correct answer with a broken
   process (shell scraping instead of using a tool, answering when it should
   refuse) is invisible in the final text.
4. **Statistical claims require paired samples** — a single observation can
   never prove "improved"; that needs N paired trials.

---

## 2. System Context

```
┌────────────────────────────────────────────────────────────────────────────┐
│                    Access layer  Channels                                   │
│       wechatmp · mcp · cli · tui · ws · webhook · manual/batch eval        │
└────────────────────────────────────┬───────────────────────────────────────┘
                                     ▼
┌────────────────────────────────────────────────────────────────────────────┐
│                 Agent core (orchestration loop)                            │
│  engine.rs · agent_engine.rs · prompt_builder · planner · agent_config     │
│  ┌───────────────┬──────────────────┬──────────────────┬───────────────┐   │
│  │ ① tool contracts │ ② observe/replay │ ③ context/compression │ ④ lifecycle  │
│  │ tools/registry │ transcript       │ compressor       │ lifecycle    │   │
│  │ registrar      │ trace replay     │ budget/disk      │ session_store │   │
│  └───────────────┴──────────────────┴──────────────────┴───────────────┘   │
└────────────────────────────────────┬───────────────────────────────────────┘
                                     │
   ┌──────────────────┬──────────────┼─────────────────┬────────────────────┐
   ▼                  ▼              ▼                 ▼                    ▼
┌────────────┐   ┌────────────┐ ┌──────────────┐  ┌─────────────┐   ┌─────────────┐
│ tools 40+  │   │ memory     │ │ model routing│  │ sandbox/security│ │ reflection │
│ shell      │   │ vector+FTS │ │ cost-aware   │  │ command_gate│   │ retrospect  │
│ browser    │   │ dreaming   │ │ fallback     │  │ shell_safety│   │ critic      │
│ file/grep  │   │ session    │ │ circuit-brk  │  │ sandbox     │   │ trajectory  │
│ computer…  │   │            │ │ quota/class  │  │ command_gate│   │             │
└────────────┘   └────────────┘ └──────────────┘  └─────────────┘   └─────────────┘
                                     │
                                     ▼
┌────────────────────────────────────────────────────────────────────────────┐
│                  Evaluation loop  Eval Harness                             │
│                                                                             │
│  layered scoring: deterministic GoalCondition → LLM Judge → human review    │
│                    ├ calibration/drift ├ multi-judge voting ├ bootstrap     │
│                    └ badcase recycle → RCA → action items → release gate   │
└────────────────────────────────────────────────────────────────────────────┘
```

### 2.1 The harness runs in three execution contexts

The layered scoring stack is not a single entry point. **Cheap deterministic
checks run inline in the hot path; expensive statistical judgments run in
batch.** All three contexts below run the *same* scoring logic — they differ
only in how many trials they run, whether an LLM judge is involved, and whether
statistics are computed.

| Context | Where | What runs | Implementation |
|---------|-------|-----------|----------------|
| ① Inline (response path) | per-turn hook in the daemon | deterministic `GoalCondition`, programmatic metrics (token/latency/cost), `RiskSignalChecker` risk scan, suspicious-badcase tagging | post-turn hooks: `scan_turn_for_badcase` (risk coarse filter → LLM judge deep review on high risk → pending pool) and `sample_turn` (production sampling) |
| ② Process gate | startup / cron | full eval harness + thresholds (`min_pass_rate` / `require_zero_p0` / `max_degradation`) → `Proceed` / `Rollback` / `Degrade` | `gateway/quality_gate.rs`, `lifecycle.rs` |
| ③ Offline batch | `syscity eval` (separate process, or daemon background job) | multi-trial + Wilson CI, LLM judge (critic / multi-judge), paired bootstrap significance, RCA, calibration | `standalone.rs` + `eval/harness.rs` |

**Boundary discipline (a core design rule):**

- The inline layer **never** computes statistics. A single observation cannot
  judge "improved / regressed"; that judgment always happens offline
  (`comparison.rs` needs paired samples).
- The inline layer is only responsible for *cheaply discovering problems and
  feeding samples into the recovery pipeline* (`recycle.rs`).
- "Batch" means *statistical judgment*, not "down time" — all three contexts
  can run while the daemon is alive. The gate's `cron_schedule` triggers ②
  in the background; ③ is both the manual `syscity eval` command and the
  daemon's background batch tasks (the auto-optimizer's eval runs are ③).

---

## 3. The Evaluation Model

### 3.1 Layered scoring

Output quality is a **multi-dimensional tuple**, not a scalar, and generative
systems have no ground truth. The layers are combined so each covers the
weakness of the ones above it:

| Layer | Mechanism | Cost / Reliability / Coverage |
|-------|-----------|------------------------------|
| 1 | Deterministic `GoalCondition` (`exit_code` / grep pattern / file existence / tool-called / tool-not-called) | cheap · reliable · narrow |
| 2 | Programmatic metrics (tokens, tool-call counts, latency, cost, retries, consecutive success) | cheap · reliable · narrow |
| 3 | LLM Judge (critic 6-dimension + multi-judge voting, temp 0.0, weighted aggregation) | wide coverage · **imprecise instrument** · needs calibration |
| 4 | Human review | closest to ground truth · expensive · sampled |

The three judging principles encoded in the implementation:

1. **Calibrate a judge before using it.** `calibration.rs` scores known-answer
   cases; `--drift` detects drift over time. Multi-judge agreement is itself a
   health metric for the judges.
2. **Score the trajectory, not just the response.** `trajectory.rs` scores
   process (tool selection, refusal behavior) which the final text hides. The
   reflection engine and `skill_scorer.rs` cover this.
3. **Regression beats absolute value.** `comparison.rs` (paired bootstrap)
   decides `Improved` / `Regressed` / `NoSignificantChange`, and the verdict is
   the release-gate hard threshold.

### 3.2 Statistical comparison

`compare_versions(old, new)` runs a paired bootstrap over per-trial results and
returns a `VersionComparison` verdict with a confidence bound. This is the
single source of truth for "did this change help" — used by the optimizer, the
shadow evaluator, the guardrails, and the release gate.

### 3.3 Judge calibration & drift

- Calibration set: known-answer cases in `evals/calibration/`.
- Drift detection: periodic `--drift` runs flag judge score drift before it
  silently corrupts verdicts.
- Multi-judge: independent models vote; a disagreement is routed to human
  review (low-confidence / conflicting cases).

---

## 4. Observation & Sampling

Evaluation is only as good as what is captured. Observation is turn-level:
each dialogue turn writes a JSON record (`~/.syscity/turns/…`, `src/observe/`)
plus a SQLite metrics row; `Trajectory` is built from turns for process
scoring.

### 4.1 What is captured

| Element | Records | Module |
|---------|---------|--------|
| User message / agent reply | `TrajectoryStep::UserMessage` / `AssistantResponse` | `reflection/trajectory.rs` |
| LLM call | `LlmRoundRecord`: provider, model, tokens, duration, TTFT, finish_reason, error, input/output | `observe/record.rs` |
| Tool call | `ObservedToolCall`: name, args, result, success, duration, error | `observe/record.rs`, `agent/turns.rs` |
| Token usage | per-round usage + cache hits | `observe/record.rs` |
| Turn metadata | session, conversation, agent_id, thread, start/finish | `observe/record.rs` |

### 4.2 Decision-level trajectories (the former diagnostic blind spots)

The internal *decisions* were historically invisible. These now have
first-class records so a badcase can be attributed:

| Decision | Record | Module |
|----------|--------|--------|
| Routing | `RouteRecord`: candidate chain, chosen model, reason, fallback occurred | `observe/record.rs`, `agent_engine.rs` (`route_log`) |
| Compression | `CompressionObservation`: trigger time, pre/post tokens, strategy, `retention_ratio` + `quality_flag` | `observe/record.rs`, `compressor.rs` |
| Planner internals | `PlanSnapshot`: plan_id, goal, step DAG (`PlanStepSnapshot`) | `observe/record.rs`, `observe/collector.rs` |
| Channel layer | `ChannelObservation`: debounce / enrich / routed agent | `observe/record.rs`, `gateway/dispatch.rs` |

### 4.3 Production turn sampling

`turn_samples` (table + `TurnSampleStore`) persists a sampled subset of real
production turns so the same scorer, compression gate, feedback aggregator and
shadow-replay pipeline can read them.

- Hook: `agent_engine.rs` `sample_turn`, fire-and-forget after a completed turn.
- Fields: turn/session/agent/conversation, input, response, model, cache hit,
  total tokens, latency, verdict, risk_signals.
- Verdict: `RiskSignalChecker::scan_turn` reuses the inline risk scorer
  (pass / flag).
- Gate: `EvalConfig.sampling.enabled` (default **off** — existing deployments
  never start writing samples unexpectedly). `sample_rate` thins the stream.

**Coverage conclusion:** offline evaluation covers the two most important
quality signals (LLM output + tool behavior), and the internal decision layers
(routing / compression / planning / channel) all have first-class replayable
trajectories. Production traffic is sampled on demand through `turn_samples`
and fed to the same scorer. What remains closed off is only what N=1 forbids
(real-time traffic splitting — see § 12).

---

## 5. The Feedback Loop

### 5.1 Like / Dislike channel

The manual feedback path is the "front door" for badcase capture:

- **Transport**: WS RPC method `feedback.vote` (`gateway/ws/feedback.rs`), not
  a new HTTP endpoint — WS is already the authenticated RPC bus.
- **Binding**: `chat.final` carries a server-generated `turn_id`; the frontend
  binds the vote to the turn, so input/output/trajectory are fully replayable.
- **Storage**: one row per `turn_id + sentiment (like/dislike) + optional
  comment`; upsert is idempotent. `FeedbackStore` (SQLite `turn_feedback`).
- **Semantics**: a down-vote becomes a `human:dislike` **pending** badcase,
  which must pass human review before becoming a regression case. A like is a
  light positive sample (calibration / preference alignment) and never feeds
  the release gate.

Design intent for N=1: with a single local user the value of the buttons is
not **volume** but **capture fidelity + accumulation** — a "this reply was
wrong" signal that would otherwise vanish becomes a reproducible candidate.
Buttons are the entry point; the real lever is badcase recycling + RCA (§ 6).

### 5.2 Rule-based aggregation (`feedback.ops`)

`build_ops_report` aggregates votes + pending badcases into a structured
diagnostic report exposed over the read-only WS method `feedback.ops`:

- vote totals (up/down), per-agent summaries, 14-day daily buckets
- down-vote summaries (turn_id / input / risk tags)
- risk clusters: each down-vote input is labeled by `RiskSignalChecker` and
  grouped by risk signal

This is deliberately rule-based (no LLM) so the report is cheap, deterministic
and auditable. The LLM-based `feedback model` (turning votes into a structured
diagnostic signal) is a **deliberate placeholder** — see § 12.

---

## 6. The Badcase Pipeline

A badcase is the concrete form of "quality definition". The pipeline is:

```
failure (online or in eval)
   → pending badcase pool        (sources: online:risk, human:dislike)
   → confirmation                (human_review, or mechanical corroboration)
   → RCA root-cause diagnosis    (rca.rs)
   → action items                (generate_action_items)
   → regression suite            (recycle.rs → evals/badcases/)
   → release gate                (quality_gate)
```

Key design points:

- **Pool, not verdict**: online flags are *candidates*. `PendingBadcaseStore`
  holds rows keyed by `source` (`online:risk` from the rule hook,
  `human:dislike` from the buttons) with `status` (`pending` / `confirmed`).
- **Quality threshold on entry**: a pending case only enters the regression
  suite after human review **or** mechanical corroboration (multiple risk
  signals / repeated trips), so garbage samples cannot pollute verdicts.
- **Governance** (`BadcaseGovernance`): the governed suite feeds the release
  gate — stale badcases expire, duplicate inputs are de-duplicated,
  high-frequency cases are demoted, and difficulty weights bias trial counts.

---

## 7. The Optimization Loop

### 7.1 Scalar optimizer

`ScalarOptimizer` (`eval/optimizer.rs`) searches the *human-declared* scalar
search space (coordinate descent / grid):

1. A candidate scalar change is generated for the default agent.
2. It is evaluated with the harness + `compare_versions` against the baseline.
3. Only an `Improved` verdict produces a patch.
4. The patch is written back via CAS (`apply_patch.rs`: `config_revision` +
   `apply_config_path` + atomic persist) and hot-reloaded for the next turn.

Design rules:

- **CAS against user edits**: writes carry the base revision and are rejected
  if the user has hand-edited the config in between.
- **Scope**: tuning targets the *global default*; per-agent `agent_overrides`
  are outside the search space.
- **Evidence, always**: a candidate is never applied on a single trial — the
  verdict needs bootstrap significance (§ 3.2).

### 7.2 Structural proposer

Structure (tool schema, prompt wording, SOP) cannot be tuned numerically, but
once written as *data* it becomes searchable. `proposer.rs` takes a badcase +
the current variant and asks the LLM for N candidate variants; each variant is
a data sample run through the same harness + verdict. The prerequisite was
re-making structures into data: tool metadata lives in the registry
(`ToolMetadata::set_metadata` updates at runtime), and prompt slots are
configurable.

### 7.3 Verdict & online shadow replay

Every candidate — scalar or structural — passes a verifier before a patch is
produced:

- **Suite-based**: `HarnessCandidateVerifier` runs the governed regression
  suite.
- **Online shadow (N=1)**: `RealTurnCandidateVerifier` replays the most recent
  sampled **production turns** through baseline vs candidate agents and runs
  `compare_versions` (`shadow_replay.rs`). Enabled by
  `optimizer.verdict.replay_shadow` (default off), it takes priority over the
  suite harness when enabled. A missing sample store degrades to no evidence
  (conservative).

---

## 8. The Release Gate

`QualityGate` (`gateway/quality_gate.rs`) runs at startup and on cron. It
combines a **regression suite** with **online signals** and produces a release
decision.

- **Criteria** (`GateCriterion`): each yields a `CriterionResult`
  (criterion, passed, actual, threshold, detail). `check()` folds them into a
  `GateResult`.
- **Release decision** (`ReleaseDecision`): `Proceed` / `Rollback` / `Degrade`,
  with `shutdown_on_failure` controlling whether a failing gate blocks startup
  or just warns.
- **Step 1b — governed badcase suite**: `BadcaseGovernance::from_config` +
  `load_governed_badcase_suite` actually load and run the regression suite at
  gate time (expiry / de-dup / demotion / difficulty weighting take effect).
- **Layered scorer in the gate**: `LayeredScorer` with a review store and
  sampling rate is injected into the harness production path, so human-review
  coverage applies to gate scoring.
- **Step 2b — compression low-retention gate**: `compression_criterion`
  counts `online:risk` low-retention flags within a window; exceeding
  `max_flagged_in_window` fails the gate. Enabled via `compression_gate`
  config (default off). A store read error fails *closed*.
- **Shadow traffic branch**: `check_with_level`'s `ShadowTraffic` arm replays
  the most recent sampled production turns (`samples_to_replay_turns` →
  `replay_shadow`) instead of an empty placeholder.

### 8.1 Rollback signals

`BaselineStore` keeps a baseline pass-rate snapshot per suite. Rollback is
triggered by **either** of two signals: a `Regressed` verdict from
`comparison.rs`, or an online-signal anomaly (rising dislike rate / risk-signal
hit rate) — the latter catches hot regressions *before* the next offline eval
would notice.

---

## 9. Guardrails & Safety

Automatic application is bounded by mechanical guardrails instead of human
approval:

1. **Locked search space.** RBAC / shell-safety / prompt-injection surfaces are
   declared non-editable; candidates are only generated inside the
   human-declared space.
2. **Cost caps.** Cost-affecting changes are bounded by `cost_guard`
   (`daily_limit_cents`, `hourly_action_limit`); over budget → reject.
3. **Canary / rollback in N=1.** The single-user canary is *shadow eval first +
   auto rollback* (`BaselineStore` + the two rollback signals in § 8.1).
4. **The eval suite is itself the guardrail.** Any candidate must pass
   multi-trial + Wilson CI + baseline comparison before application.
5. **Circuit breaker / pause.** On consecutive `Regressed` verdicts, sustained
   cost overruns, or online-signal degradation, the optimizer auto-pauses and
   alerts; only human intervention resumes it (escape hatch
   `eval.optimizer.resume`). This prevents a rollback loop from churning.

### 9.1 Config convention

New gates and sampling switches default **off** (`#[serde(default)]`), so
existing deployments are never silently affected. Enabling them is an explicit,
documented operator choice (see § 12 for how to think about thresholds).

---

## 10. Configuration Surface

### 10.1 Where knobs live

| Entry point | Purpose | Typical cases |
|-------------|---------|---------------|
| `config.toml` (`~/.syscity/config.toml`) | persistent config; deserialized into `AgentConfig` / `EvalConfig` / `QualityGateConfig` | dialogue behavior, models, cost, eval |
| `syscity eval` CLI | per-run overrides (trials / sampling / provider / model) | running an eval |
| Code defaults + runtime routing API | `AgentConfig::default()` fallback; `model_router/router/admin.rs` runtime provider / fallback changes | fallback values, no-restart routing |

### 10.2 The eval/quality surface

Key sections in `EvalConfig` / `QualityGateConfig`:

- `eval.sampling` (`OnlineSamplingConfig`): production turn sampling; `enabled`
  default false, `sample_rate` in `[0.0, 1.0]` (0.0 = keep every turn when
  enabled).
- `eval.compression_quality`: `enabled`, `min_retention_ratio` — quantifies
  compression retention and flags low-retention compressions as online risk.
- `eval.optimizer`: `ScalarOptimizerConfig` (`cadence`, `max_steps`, `delta`,
  `temperature_bounds`, `guardrails`, `verdict`).
- `eval.optimizer.verdict`: `OptimizerVerdictConfig` (`enabled`, `suite`,
  `trials`, `bootstrap_iterations`, `confidence_level`, `replay_shadow`).
- `eval.badcase_governance`, `eval.human_review.sampling_rate`,
  `eval.online_monitoring` (LLM-judge risk threshold / judge model),
  `eval.proposer` (structural candidates).
- `quality_gate`: `QualityGateConfig` with suite thresholds
  (`min_pass_rate` / `require_zero_p0` / `max_degradation`), the `compression_gate`
  option, and `shutdown_on_failure`.

### 10.3 Tuning discipline

- **Parameters are the last lever**: tool design > prompts > routing > retrieval
  > parameters. If a badcase points at tool misuse, tuning `temperature` is
  treating the symptom.
- **Every change must pass § 7 verification** (multi-trial + Wilson CI +
  baseline comparison). A single LLM output is noisy; "adjust by looking" is
  being fooled by variance.
- **Judge temperature is deliberately fixed at 0.0** for deterministic scoring;
  if you want to change scoring behavior, change the calibration set or the
  scoring rubric, not the temperature knob.

---

## 11. Implemented Mechanisms — Summary

| Mechanism | Where | Notes |
|-----------|-------|-------|
| Multi-trial eval harness + Wilson CI + dimension averaging + early stop | `eval/harness.rs` | |
| Deterministic scoring | `goal/condition.rs`, `eval/scorer.rs` | exit code / pattern / file / tool |
| Risk coarse filter | `eval/scorer.rs` (`RiskSignalChecker`) | sensitive words / too-short / too-many-tools |
| LLM judge + multi-judge voting | `reflection/critic.rs`, `eval/multi_judge.rs` | temp 0.0, weighted aggregation |
| Calibration & drift | `eval/calibration.rs` | known-answer cases |
| Statistical comparison | `eval/comparison.rs` | paired bootstrap |
| RCA pipeline | `eval/rca.rs` | |
| Badcase recycle → regression suite | `eval/recycle.rs` | |
| Human review routing | `eval/human_review.rs` | low-confidence / conflicting cases |
| Skill-specific scoring | `eval/skill_scorer.rs` | trigger / execution / quality / resilience |
| Pending badcase pool | `eval/pending_badcase.rs` | sources: `online:risk`, `human:dislike` |
| Like/Dislike feedback | `gateway/feedback.rs`, `gateway/ws/feedback.rs` (`feedback.vote`) | |
| Rule-based feedback aggregation | `eval/feedback_ops.rs`, WS `feedback.ops` | read-only, no LLM |
| Production turn sampling | `eval/sample_store.rs` (`turn_samples`), `agent_engine.rs` `sample_turn` | default off |
| Scalar optimizer + CAS apply + hot reload | `eval/optimizer.rs`, `eval/apply_patch.rs` | only `Improved` produces patches |
| Structural proposer | `eval/proposer.rs` | LLM variants as data samples |
| Online shadow replay (N=1) | `gateway/shadow_replay.rs`, `eval/guardrail.rs` (`RealTurnCandidateVerifier`) | `replay_shadow` default off |
| Compression low-retention gate | `eval/compression_gate.rs` (`compression_criterion`), `quality_gate.rs` Step 2b | default off |
| Release gate + governance + baseline rollback | `gateway/quality_gate.rs`, `gateway/lifecycle.rs` | `Proceed`/`Rollback`/`Degrade` |
| Reflection engine | `agent/reflection/` | background trajectory self-critique |
| Eval dashboard trends | WS `eval.dashboard` (14-day), web Eval page | |

---

## 12. Points to Consider

These are the open items — some are deliberate placeholders, some are
N=1 architectural limits, some are "implemented but not yet exercised".

### 12.1 Deliberate placeholders

- **LLM `feedback model`** — `feedback.ops` is rule-based by design (cheap,
  deterministic, auditable). An LLM that folds votes into a structured
  diagnostic signal is still a placeholder. Consider adding it only if the
  rule-based clusters turn out not to be actionable. If added, it should sit
  behind the same "judge is an imprecise instrument" discipline (calibrate,
  sample, verify) rather than being trusted raw.

### 12.2 N=1 architectural limits

- **Real-time traffic splitting / A-B / canary.** The current shadow is
  *offline replay* (N=1): sampled real turns are replayed through candidate vs
  baseline and compared. Real per-version traffic splitting (gray / canary)
  has no statistical power for a single user and is intentionally not built.
  It requires a fleet (§ 12.5).
- **Statistical power of N=1 replay.** `replay_shadow` runs with a small number
  of trials over the recent sample window. The verdict is only as strong as the
  sample size and the `confidence_level` configured. If the window is mostly
  empty or the turns are homogeneous, the verdict is weak — and the system
  conservatively treats "no evidence" as no change.

### 12.3 Implemented but not yet exercised / tuned

- **Default-off gates.** `eval.sampling.enabled`, `compression_gate`,
  `optimizer.verdict.replay_shadow` are implemented but off. Before enabling:
  - Decide `max_flagged_in_window` / `window_ms` for the compression gate from
    observed flag rates, not guesses.
  - Confirm `sample_rate` semantics: 0.0 keeps *every* turn — for a chatty
    deployment that may be far more rows than intended; a fraction in
    `(0.0, 1.0)` thins the stream but the per-turn skip is cheap, not
    cryptographically fair — decide whether approximate thinning is
    acceptable.
  - Consider retention / privacy of `turn_samples`: sampled turns contain real
    user content (input/output). No TTL is defined; add one if sampling is
    enabled in production.
- **Threshold choice for the compression gate** depends on the distribution of
  `online:risk` flags, which is only knowable after sampling runs a while.
- **Calibration-set maintenance.** The judge calibration set is human
  maintained; a stale set silently degrades verdict quality. There is no
  automated "flag the judge, refresh the set" loop yet.

### 12.4 Design properties worth re-examining

- **Fire-and-forget sampling.** `sample_turn` runs post-turn, spawned and
  dropped on failure (logged, not propagated) to keep the hot path safe. Under
  load this can silently lose samples. Consider a bounded queue + batch writer
  if sampling is enabled and rows matter.
- **Fail-closed gates.** The compression gate fails closed on store read error.
  That is safe (never pass on missing data) but means a storage hiccup at gate
  time blocks/degrades a release. Decide whether gate-time store outages should
  be `Degrade` rather than `Rollback`.
- **Shadow gate reads the whole recent window.** `ShadowTraffic` replays up to
  a fixed number of recent samples with N=1 trials each; each replay is a real
  model run, so this is real cost at gate time. A large window or slow model
  can make the gate slow. Consider a cap on total turns replayed.

### 12.5 Future: fleet / federated optimization

The current design is local-first and single-user. The *only* reason the
"missing" items above are missing is N=1. A future opt-in telemetry / fleet
layer would change the math:

- **Federated badcases**: one user's confirmed badcase (anonymized) enters a
  shared regression suite — network effect on guardrails.
- **Fleet A/B + canary**: gray a candidate config/prompt/tool over a sampled
  user subset, compute Wilson CI with real N, get trustworthy verdicts.
- **Preference alignment**: aggregated like/dislike + human-review judgments →
  preference datasets.
- **Calibration-set evolution**: high-confidence fleet judgments feed
  `calibration.rs`; drift detection uses real distributions.

Hard constraints to satisfy before this is real:

1. **Privacy is a hard constraint.** Trajectories contain user content
   (command output, file contents, chat text). Collection must be opt-in,
   per-field consent, and de-identified (PII / tool output / content reduced to
   statistics). This determines what granularity can be collected at all.
2. **Local / self-hosted tension.** A fleet only exists when users opt into
   telemetry; self-hosters typically send the least. This is a distribution
   model choice (central SaaS vs opt-in sharing network), not a pure technical
   problem.
3. **Environment-specific badcases must be normalized.** A user's badcase may
   depend on their shell / files / environment and cannot be aggregated
   verbatim. Normalize to the *schema / task-type* level ("this task class
   systematically gets tool-call ordering wrong"), not "this user's specific
   command was wrong".

The architecture direction is **data/metric federation, optimization
centralized**: clients report de-identified data and eval metrics; the backend
runs the optimizer and distributes config / prompt / badcase updates. No model
weights are trained on client devices. The existing `feedback.vote` pipeline
and `human_review` judgments are already the ingestion entry points; a
federation layer is just an optional transport on top.

---

## Appendix: Key Module Index

```
src/core/engine.rs                  engine (orchestration loop)
src/agent/agent_engine.rs           agent orchestration, post-turn hooks
src/agent/                          lifecycle · context · session · reflection
src/agent/reflection/               critic / trajectory / retrospect
src/tools/                          tool registry & implementation (40+)
src/model_router/                   routing · cost awareness · circuit breaker · quotas
src/observe/                        per-turn observation (TurnRecord / LLM rounds / tools)
src/providers/                      CompletionRequest (temperature / max_tokens / top_p)
src/memory/                         hybrid retrieval · dreaming
src/goal/condition.rs               deterministic GoalCondition
src/eval/harness.rs                 Eval Harness (multi-trial + Wilson CI)
src/eval/scorer.rs                  coarse RiskSignal → LLM judge
src/eval/multi_judge.rs             multi-judge weighted aggregation
src/eval/calibration.rs             judge calibration + drift
src/eval/comparison.rs              version comparison (bootstrap significance)
src/eval/rca.rs                     badcase root-cause diagnosis
src/eval/recycle.rs                 badcase recycle → regression suite
src/eval/human_review.rs            human review
src/eval/pending_badcase.rs         pending badcase pool (online:risk / human:dislike)
src/eval/sample_store.rs            production turn samples (turn_samples)
src/eval/compression_gate.rs        compression low-retention gate
src/eval/feedback_ops.rs            rule-based feedback aggregation
src/eval/optimizer.rs               scalar optimizer
src/eval/proposer.rs                structural (LLM) proposer
src/eval/apply_patch.rs             CAS config write-back + hot reload
src/eval/decision_trace.rs          optimizer decision audit trail
src/eval/guardrail.rs               circuit breaker / shadow evaluators / RealTurnCandidateVerifier
src/gateway/shadow_replay.rs        N=1 online replay shadow
src/gateway/quality_gate.rs         release gate
src/gateway/feedback.rs             feedback store (turn_feedback)
src/gateway/ws/feedback.rs          feedback.vote / feedback.ops WS handlers
src/gateway/lifecycle.rs            daemon lifecycle, gate wiring
evals/                              capability / adversarial / regression /
                                    calibration / skills / badcases / suites
```
