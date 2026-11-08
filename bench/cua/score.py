#!/usr/bin/env python3
"""Read a Cua Bench run and report both scores, without flattering either.

Two numbers matter and they measure different things:

* **raw** — `avg_reward` over every variant, straight out of `summary.json`.
  This is the dataset score. Quote it.
* **reachable** — accuracy over the variants this environment can actually
  solve, i.e. minus the whole tasks whose *official oracle* fails on every one
  of its variants here. This is the model's score at the environment's ceiling.

The second number is only as honest as its evidence, and `bench/cua/ceiling.json`
is where that evidence lives. A task is excluded only when **all three** hold:

1. the oracle scored 0 on **every** variant the run contains (a `null` is *not
   measured*, and blocks the exclusion rather than counting as zero);
2. the model scored 1 on **no** variant of it;
3. the failure has a mechanism that is understood, recorded in the file.

Every one of those is a guard against a specific way of lying with this number:

* per-variant exclusion would let a run drop exactly the variants the model
  failed — that is cherry-picking, so exclusion is per task and all-or-nothing;
* a model success proves the environment reached the task, so it vetoes
  exclusion — otherwise a broken oracle would be counted as an environment
  limit and the model would be credited for work it did not do;
* an unmeasured oracle is not a ceiling, so it stays in the denominator.

`date-picker` is why this is strict. Its oracle scored 0 on all five variants —
it types into `input[type=date]`, which WebKitGTK ignores — and a looser rule
would have excluded the task and reported a better score for it. But the model
solved v4 by clicking the picker's spin buttons, so the picker works and the
oracle is what fails. The task stays in.

    python3 bench/cua/score.py <run-dir> [--ceiling bench/cua/ceiling.json]
    python3 bench/cua/score.py <run-dir> --json
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass, field


@dataclass
class Ceiling:
    """The environment's declared unreachable tasks, with their evidence."""

    #: task -> {variant: oracle_reward or None}
    oracle: dict = field(default_factory=dict)

    @classmethod
    def load(cls, path):
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        oracle = {}
        for task, entry in (data.get("unreachable_tasks") or {}).items():
            rewards = {}
            for variant, value in (entry.get("oracle_reward") or {}).items():
                # JSON object keys are strings; accept both spellings so a
                # hand-edited file cannot silently miss a variant.
                rewards[int(variant)] = value
            oracle[task] = rewards
        return cls(oracle=oracle)

    def verdict(self, task, model_rewards):
        """Decide a declared task's fate against the run.

        Returns `(state, why)` with state one of:

        * `"exclude"` — every guard held; the task is outside the ceiling.
        * `"model-solved"` — the model passed a variant, so the task is
          reachable; the declaration is wrong and is reported, not applied.
        * `"unmeasured"` — some variant has no oracle run recorded.
        * `"incomplete"` — the oracle passed a variant, so the task is not
          wholly unreachable.
        """
        declared = self.oracle.get(task) or {}
        passed = sorted(v for v, reward in model_rewards.items() if reward == 1.0)
        if passed:
            return "model-solved", passed

        missing = sorted(v for v, reward in model_rewards.items() if declared.get(v) is None)
        if missing:
            return "unmeasured", missing

        reachable = sorted(v for v, reward in model_rewards.items() if declared.get(v) != 0.0)
        if reachable:
            return "incomplete", reachable

        return "exclude", sorted(model_rewards)


def load_summary(run_dir):
    with open(f"{run_dir}/summary.json", encoding="utf-8") as handle:
        return json.load(handle)


@dataclass
class Report:
    total: int
    scored: int
    passed: int
    raw: float
    reachable_total: int
    reachable_passed: int
    excluded: list
    warnings: list
    errored: list

    @property
    def reachable(self):
        if not self.reachable_total:
            return None
        return self.reachable_passed / self.reachable_total

    def as_dict(self):
        return {
            "total_variants": self.total,
            "scored_variants": self.scored,
            "passed": self.passed,
            "raw": round(self.raw, 4),
            "reachable": None if self.reachable is None else round(self.reachable, 4),
            "reachable_variants": self.reachable_total,
            "reachable_passed": self.reachable_passed,
            "excluded_tasks": self.excluded,
            "declaration_warnings": self.warnings,
            "errored": self.errored,
        }


def report(summary, ceiling):
    results = summary.get("results") or []
    rows = [row for row in results if row.get("reward") is not None]
    passed = sum(1 for row in rows if row["reward"] == 1.0)
    raw = summary.get("avg_reward")
    if raw is None:
        raw = (passed / len(rows)) if rows else 0.0

    by_task = {}
    for row in rows:
        by_task.setdefault(row["task"], {})[int(row["variant"])] = row["reward"]

    excluded, warnings = set(), []
    for task in ceiling.oracle:
        if task not in by_task:
            continue  # declared, but this run did not include it
        state, detail = ceiling.verdict(task, by_task[task])
        if state == "exclude":
            excluded.add(task)
        elif state == "model-solved":
            warnings.append(
                "{}: declared unreachable but the model solved v{} — not "
                "excluded".format(task, ", v".join(str(v) for v in detail))
            )
        elif state == "unmeasured":
            warnings.append(
                "{}: no oracle recorded for v{} — not excluded (unmeasured is "
                "not unreachable)".format(task, ", v".join(str(v) for v in detail))
            )
        else:
            warnings.append(
                "{}: oracle passed v{}, so the task is not wholly unreachable — "
                "not excluded".format(task, ", v".join(str(v) for v in detail))
            )

    reachable_total = reachable_passed = 0
    for task, rewards in by_task.items():
        if task in excluded:
            continue
        for reward in rewards.values():
            reachable_total += 1
            if reward == 1.0:
                reachable_passed += 1

    errored = [
        "{} v{}: {}".format(row["task"], row["variant"], row.get("error") or row.get("status"))
        for row in results
        if row.get("reward") is None
    ]
    return Report(
        total=summary.get("total", len(results)),
        scored=len(rows),
        passed=passed,
        raw=raw,
        reachable_total=reachable_total,
        reachable_passed=reachable_passed,
        excluded=sorted(excluded),
        warnings=warnings,
        errored=errored,
    )


def render(rep):
    lines = [
        "raw        {}/{} = {:.3f}   (dataset score — quote this)".format(
            rep.passed, rep.scored, rep.raw
        )
    ]
    if rep.reachable is None:
        lines.append("reachable  n/a (every scored variant was excluded)")
    else:
        lines.append(
            "reachable  {}/{} = {:.3f}   (model, with the environment ceiling applied)".format(
                rep.reachable_passed, rep.reachable_total, rep.reachable
            )
        )
    if rep.excluded:
        lines.append(
            "excluded (oracle 0 on every variant, model solved none): {}".format(
                ", ".join(rep.excluded)
            )
        )
    for warning in rep.warnings:
        lines.append("warning: " + warning)
    if rep.errored:
        lines.append("errored (not scored): {}".format(", ".join(rep.errored)))
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("run_dir", help="directory holding summary.json")
    parser.add_argument(
        "--ceiling",
        default=__file__.rsplit("/", 1)[0] + "/ceiling.json",
        help="environment ceiling declaration (default: bench/cua/ceiling.json)",
    )
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    args = parser.parse_args(argv)

    rep = report(load_summary(args.run_dir), Ceiling.load(args.ceiling))
    print(json.dumps(rep.as_dict(), indent=2) if args.json else render(rep))
    return 0


if __name__ == "__main__":
    sys.exit(main())
