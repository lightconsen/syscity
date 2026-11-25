#!/usr/bin/env python3
"""Import a GAIA split into the local eval harness.

Defaults to the validation split's Level 1 (53 questions, the M2 baseline).
`--split test` imports the held-out test split instead — all 301 questions,
Levels 1-3 — but its `Final answer` column is `?` for every row: the GAIA
dataset card says "a test set with private answers", so the test split can be
fetched for inspection and never scored locally. Submitting it means running
the official leaderboard harness.

Produces two files, both gitignored because the GAIA terms of use do not allow
redistribution:

    evals/capability/gaia_<split>_l<levels>.yaml   tasks, one per question
    evals/capability/gaia_<split>_l<levels>_answers.json   ground truth by id

plus the referenced attachments, cached outside the repo (default
`~/.cache/syscity-bench/gaia/files`, override with `--cache` or
`$SYSCITY_GAIA_CACHE`).

Prerequisites
-------------
The dataset is gated: accept the terms at
<https://huggingface.co/datasets/gaia-benchmark/GAIA> with the account whose
token you use. Then, once per machine, install the parquet reader the importer
needs (Python has no stdlib parquet support):

    python3 -m venv ~/.cache/syscity-bench/gaia/venv
    ~/.cache/syscity-bench/gaia/venv/bin/pip install pyarrow pyyaml
    ~/.cache/syscity-bench/gaia/venv/bin/python evals/importers/gaia.py

The token is read from `--token`, `$HF_TOKEN`, `secrets/hf.secret` (line 2, the
repo-local store), or `~/.cache/huggingface/token` (in that order).
`$HTTPS_PROXY` is honoured, as the HF endpoints are often unreachable
otherwise.

Only the account that accepted the dataset's terms can download it. The
`~/.cache/huggingface/token` fallback is the one that bites: it holds whatever
account last ran `hf auth login`, which is not necessarily that account, and the
resulting 403 reads like "you never accepted the terms" when you did.

Why a generated file rather than a checked-in one
-------------------------------------------------
GAIA ships under terms that forbid redistribution, and every attachment path is
machine-specific. So the task file is a build artifact: regenerate it with this
script, and treat `evals/suites/gaia_l1.yaml` (checked in) as the entry point.

Scoring is not done here — each task's condition shells out to
`gaia_score.py`, which wraps the official `question_scorer`.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
HF_BASE = "https://huggingface.co/datasets/gaia-benchmark/GAIA/resolve/main/"

# Per-split shape, from the dataset itself. A deviation means GAIA changed
# under us and the ledger numbers stop being comparable, so it fails closed.
#   validation: 165 rows = 53 L1 + 86 L2 + 26 L3
#   test:       301 rows = 93 L1 + 159 L2 + 49 L3   ← the submittable split
SPLITS = {
    "validation": {1: 53, 2: 86, 3: 26},
    "test": {1: 93, 2: 159, 3: 49},
}

ANSWER_INSTRUCTION = (
    "When you are done, end your reply with a single line:\n"
    "FINAL ANSWER: <your answer>"
)


def cache_root(explicit: str | None) -> Path:
    if explicit:
        return Path(explicit).expanduser()
    env = os.environ.get("SYSCITY_GAIA_CACHE")
    if env:
        return Path(env).expanduser()
    return Path.home() / ".cache" / "syscity-bench" / "gaia"


def read_token(explicit: str | None) -> str | None:
    if explicit:
        return explicit.strip()
    env = os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN")
    if env:
        return env.strip()
    # `secrets/hf.secret` is the repo-local store (gitignored by `secrets/`);
    # line 1 is the account email, line 2 the token. It sits before the global
    # cache because the cache usually belongs to whichever account last ran
    # `hf auth login`, and that account is not necessarily authorized for a
    # gated dataset — silently falling back to it turned a few seconds of
    # setup into a 403 that reads like "you never accepted the terms".
    local = REPO_ROOT / "secrets" / "hf.secret"
    if local.is_file():
        lines = local.read_text(encoding="utf-8").splitlines()
        if len(lines) >= 2 and lines[1].strip():
            return lines[1].strip()
    token_file = Path.home() / ".cache" / "huggingface" / "token"
    if token_file.is_file():
        return token_file.read_text(encoding="utf-8").strip()
    return None


def fetch(url: str, dest: Path, token: str | None) -> None:
    """Download `url` to `dest`, failing closed on an HTML or JSON error body."""
    headers = {"User-Agent": "syscity-gaia-importer/1"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, headers=headers)
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")
    try:
        with urllib.request.urlopen(req, timeout=120) as resp, tmp.open("wb") as f:
            f.write(resp.read())
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")[:400]
        tmp.unlink(missing_ok=True)
        if e.code in (401, 403):
            raise SystemExit(
                f"Cannot download {url}\n  HTTP {e.code}: {body.strip()}\n"
                "  The GAIA dataset is gated: accept the terms at\n"
                "  https://huggingface.co/datasets/gaia-benchmark/GAIA\n"
                "  with the account whose token is being used."
            )
        raise SystemExit(f"Cannot download {url}\n  HTTP {e.code}: {body.strip()}")
    except urllib.error.URLError as e:
        tmp.unlink(missing_ok=True)
        raise SystemExit(
            f"Cannot download {url}\n  {e.reason}\n"
            "  If the network needs a proxy: export https_proxy=http://127.0.0.1:1087"
        )
    tmp.replace(dest)


def load_rows(metadata: Path) -> list[dict]:
    try:
        import pyarrow.parquet as pq  # noqa: PLC0415 — optional dependency
    except ImportError:
        raise SystemExit(
            "pyarrow is required to read the GAIA metadata parquet file.\n"
            "  python3 -m venv ~/.cache/syscity-bench/gaia/venv\n"
            "  ~/.cache/syscity-bench/gaia/venv/bin/pip install pyarrow\n"
            "  ~/.cache/syscity-bench/gaia/venv/bin/python evals/importers/gaia.py"
        )
    table = pq.read_table(metadata)
    return table.to_pylist()


def task_input(question: str, attachment: Path | None) -> str:
    """The user message: the question, where its file is, and how to answer."""
    parts = [question.strip()]
    if attachment is not None:
        parts.append(f"The attached file is available at: {attachment}")
    parts.append(ANSWER_INSTRUCTION)
    return "\n\n".join(parts)


def build_tasks(
    rows: list[dict],
    files_dir: Path,
    token: str | None,
    *,
    download_files: bool,
    limit: int | None,
    split: str = "validation",
    levels: tuple[int, ...] = (1,),
    slug_prefix: str = "gaia_l1",
    answers_path: Path | None = None,
) -> tuple[list[dict], dict]:
    tasks: list[dict] = []
    answers: dict = {}
    seen: set[str] = set()
    withheld = 0
    scorer = (REPO_ROOT / "evals" / "importers" / "gaia_score.py").resolve()
    # Each scoring condition has to name *its own* answers file. The wrapper's
    # default is the validation baseline, so a test-split import that omitted
    # this would score against the wrong ground truth — and the wrapper's
    # "no ground truth" verdict (exit 2) would read as a model miss.
    scorer_args = f"python3 {scorer} --task {{task_id}} --answer ${{trial_dir}}/response.txt"
    answers_flag = f" --answers {answers_path}" if answers_path is not None else ""

    selected = [r for r in rows if int(r["Level"]) in levels]
    selected.sort(key=lambda r: (int(r["Level"]), r["task_id"]))
    if limit is not None:
        selected = selected[:limit]

    for row in selected:
        task_id = row["task_id"]
        level = int(row["Level"])
        # The prefix is the caller's, so the validation Level-1 suite keeps the
        # ids its ledger already records (`gaia_l1_<task_id[:8]>`); a test-split
        # import gets its own namespace.
        slug = f"{slug_prefix}_{task_id[:8]}"
        if slug in seen:
            raise SystemExit(f"duplicate task slug {slug} ({task_id})")
        seen.add(slug)

        attachment = None
        remote = (row.get("file_path") or "").strip()
        if remote:
            attachment = (files_dir / Path(remote).name).resolve()
            if download_files and not attachment.exists():
                print(f"  downloading attachment for {slug}: {attachment.name}")
                fetch(HF_BASE + remote, attachment, token)
        expected = (row.get("Final answer") or "").strip()
        if not expected:
            raise SystemExit(f"task {task_id} has no ground truth")
        # `?` is the official "answer withheld / not a single answer" placeholder.
        # A split made entirely of them is the private-answer test set: importing
        # it would produce a suite that can only ever score 0%, and that 0% would
        # read as a model failure rather than a missing ground truth.
        if expected == "?":
            withheld += 1

        tasks.append(
            {
                "id": slug,
                "description": f"GAIA {split} Level {level} · task_id={task_id}",
                "difficulty": "medium",
                "coverage": ["benchmark", "gaia"],
                "expected_behavior": (
                    "End the reply with `FINAL ANSWER: <answer>`; the official "
                    "GAIA question_scorer decides pass/fail."
                ),
                "input": task_input(row["Question"], attachment),
                "conditions": [
                    {
                        "type": "exit_code",
                        "command": (
                            scorer_args.replace("{task_id}", task_id) + answers_flag
                        ),
                        "expected": 0,
                    }
                ],
            }
        )
        answers[task_id] = {
            "id": slug,
            "level": level,
            "answer": expected,
            "has_attachment": bool(remote),
        }

    if tasks and withheld == len(tasks):
        raise SystemExit(
            f"all {len(tasks)} {split} tasks have a withheld ground truth (`?`).\n"
            "  The GAIA dataset card: 'a test set with private answers'. Only the\n"
            "  validation split can be scored locally; the test split is scored by\n"
            "  the official leaderboard harness. Point --levels/--split at\n"
            "  validation if you want a number."
        )

    return tasks, answers


YAML_HEADER = """\
# GAIA — GENERATED FILE, DO NOT EDIT.
#
# Regenerate with:
#   ~/.cache/syscity-bench/gaia/venv/bin/python evals/importers/gaia.py [--split test]
#
# Not committed: the GAIA terms of use forbid redistribution, and the
# attachment paths are machine-specific. Ground truth lives beside this file in
# `<name>_answers.json`; scoring is the official GAIA `question_scorer`
# (vendored in `evals/importers/gaia_scorer.py`) invoked per trial by
# `gaia_score.py`.
#
# Every task asks the agent to finish with `FINAL ANSWER: <answer>`; the
# wrapper scores that line, or the whole message when the marker is absent.
"""


def write_yaml(path: Path, tasks: list[dict]) -> None:
    try:
        import yaml  # noqa: PLC0415 — optional dependency, mirrors pyarrow
    except ImportError:
        raise SystemExit(
            "PyYAML is required to write the task file:\n"
            "  ~/.cache/syscity-bench/gaia/venv/bin/pip install pyyaml"
        )
    body = yaml.safe_dump(
        {"tasks": tasks}, allow_unicode=True, sort_keys=False, width=200
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(YAML_HEADER + "\n" + body, encoding="utf-8")


def self_test() -> int:
    """Exercise the importer end to end on synthetic rows (no GAIA access)."""
    import pyarrow as pa
    import pyarrow.parquet as pq
    import yaml

    rows = [
        {
            "task_id": "00000000-0000-0000-0000-000000000001",
            "Question": "What is 2 + 2?",
            "Level": 1,
            "Final answer": "4",
            "file_name": "",
            "file_path": "",
        },
        {
            "task_id": "bbbbbbbb-0000-0000-0000-000000000002",
            "Question": "Which city is named in the attachment?",
            "Level": 1,
            "Final answer": "Paris",
            "file_name": "x.txt",
            "file_path": "2023/validation/x.txt",
        },
        {
            "task_id": "cccccccc-0000-0000-0000-000000000003",
            "Question": "Level 2 row — must be filtered out.",
            "Level": 2,
            "Final answer": "nope",
            "file_name": "",
            "file_path": "",
        },
    ]
    with tempfile.TemporaryDirectory() as tmp:
        tmpdir = Path(tmp)
        parquet = tmpdir / "metadata.parquet"
        pq.write_table(pa.Table.from_pylist(rows), parquet)

        tasks, answers = build_tasks(
            load_rows(parquet),
            tmpdir / "files",
            None,
            download_files=False,
            limit=None,
            split="validation",
            levels=(1,),
            slug_prefix="gaia_l1",
            answers_path=tmpdir / "answers.json",
        )
        assert len(tasks) == 2, tasks
        assert [t["id"] for t in tasks] == ["gaia_l1_00000000", "gaia_l1_bbbbbbbb"]
        # Every condition must name its own answers file: the wrapper's default
        # is the validation baseline, which would silently score the wrong
        # ground truth for any other split.
        assert any(
            "answers.json" in t["conditions"][0]["command"] for t in tasks
        ), tasks[0]["conditions"][0]["command"]
        assert "${trial_dir}" in tasks[0]["conditions"][0]["command"]

        # A multi-level import keeps every level and namespaces by split.
        all_levels, _ = build_tasks(
            load_rows(parquet),
            tmpdir / "files",
            None,
            download_files=False,
            limit=None,
            split="test",
            levels=(1, 2),
            slug_prefix="gaia_t12",
        )
        assert [t["id"] for t in all_levels] == [
            "gaia_t12_00000000",
            "gaia_t12_bbbbbbbb",
            "gaia_t12_cccccccc",
        ], all_levels

        yaml_path = tmpdir / "out.yaml"
        write_yaml(yaml_path, tasks)
        parsed = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
        assert len(parsed["tasks"]) == 2
        first = parsed["tasks"][0]
        assert first["conditions"][0]["type"] == "exit_code"
        assert "FINAL ANSWER:" in first["input"]
        assert answers["00000000-0000-0000-0000-000000000001"]["answer"] == "4"
        assert "x.txt" in parsed["tasks"][1]["input"], parsed["tasks"][1]["input"]
    print("gaia.py self-test passed")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--out",
        type=Path,
        help="generated task file (default: evals/capability/gaia_<split>.yaml)",
    )
    parser.add_argument(
        "--answers-out",
        type=Path,
        help="ground-truth JSON (default: evals/capability/gaia_<split>_answers.json)",
    )
    parser.add_argument(
        "--split",
        choices=sorted(SPLITS),
        default="validation",
        help="which GAIA split to import (default: %(default)s)",
    )
    parser.add_argument(
        "--levels",
        default=None,
        help="comma-separated levels to import, e.g. '1' or '1,2,3' "
        "(default: validation=L1, test=1,2,3)",
    )
    parser.add_argument(
        "--slug",
        help="task-id prefix (default: validation L1 only='gaia_l1', else 'gaia_<split>')",
    )
    parser.add_argument(
        "--cache",
        help="cache root for the metadata parquet and attachments "
        "(default: $SYSCITY_GAIA_CACHE or ~/.cache/syscity-bench/gaia)",
    )
    parser.add_argument(
        "--metadata",
        type=Path,
        help="use an existing metadata parquet instead of downloading it",
    )
    parser.add_argument(
        "--token",
        help="HF token (default: $HF_TOKEN, then ~/.cache/huggingface/token)",
    )
    parser.add_argument(
        "--no-files",
        action="store_true",
        help="skip attachment downloads; tasks still reference the expected path",
    )
    parser.add_argument(
        "--limit", type=int, help="import only the first N tasks (smoke run)"
    )
    parser.add_argument(
        "--allow-count-drift",
        action="store_true",
        help="do not fail when the row count differs from the published shape",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="exercise the importer on synthetic rows and exit",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    split = args.split
    if args.levels is not None:
        levels = tuple(int(x) for x in args.levels.replace(" ", "").split(",") if x)
        unknown = [l for l in levels if l not in SPLITS[split]]
        if unknown:
            parser.error(f"split {split!r} has no level(s) {unknown}")
    elif split == "validation":
        # The committed M2 baseline is validation Level 1 only; keep the default
        # so re-running the importer reproduces the ledger's suite unchanged.
        levels = (1,)
    else:
        levels = tuple(sorted(SPLITS[split]))

    # One name for the suite, its files, and its task ids. `gaia_l1` is the
    # committed M2 baseline and keeps the id its ledger records; every other
    # combination is `gaia_<split>_l<levels>` (`gaia_test_l123`), so a badcase
    # id says exactly which split and levels it came from.
    file_stem = (
        "gaia_l1"
        if split == "validation" and levels == (1,)
        else f"gaia_{split}_l" + "".join(map(str, levels))
    )
    slug_prefix = args.slug or file_stem
    out = args.out or REPO_ROOT / "evals" / "capability" / f"{file_stem}.yaml"
    answers_out = args.answers_out or (
        REPO_ROOT / "evals" / "capability" / f"{file_stem}_answers.json"
    )

    cache = cache_root(args.cache)
    metadata = args.metadata or cache / "metadata" / f"{split}.parquet"
    token = read_token(args.token)
    if token is None:
        print("warning: no HF token found; gated downloads will fail", file=sys.stderr)

    if not metadata.exists():
        print(f"downloading 2023/{split}/metadata.parquet -> {metadata}")
        fetch(HF_BASE + f"2023/{split}/metadata.parquet", metadata, token)

    rows = load_rows(metadata)
    tasks, answers = build_tasks(
        rows,
        cache / "files",
        token,
        download_files=not args.no_files,
        limit=args.limit,
        split=split,
        levels=levels,
        slug_prefix=slug_prefix,
        answers_path=answers_out.resolve(),
    )
    expected_count = sum(SPLITS[split][l] for l in levels)
    if (
        args.limit is None
        and len(tasks) != expected_count
        and not args.allow_count_drift
    ):
        raise SystemExit(
            f"expected {expected_count} Level-{'/'.join(map(str, levels))} tasks "
            f"from the {split} split, built {len(tasks)}. "
            "The dataset may have been updated — re-check before comparing "
            "against the ledger, then pass --allow-count-drift to proceed."
        )

    write_yaml(out, tasks)
    answers_out.parent.mkdir(parents=True, exist_ok=True)
    answers_out.write_text(
        json.dumps(answers, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )

    with_files = sum(1 for r in answers.values() if r["has_attachment"])
    print(f"wrote {len(tasks)} tasks -> {out}")
    print(f"  {with_files} with attachments, cached under {cache / 'files'}")
    print(f"wrote ground truth -> {answers_out}")
    print()
    if split == "test":
        print("The test split cannot be scored locally (answers are private). Generate")
        print("it only to inspect the agent's traces; submit the answers to the GAIA")
        print("leaderboard Space for a score.")
        return 0
    print("Run it with:")
    if file_stem == "gaia_l1":
        print(f"  syscity eval run {file_stem} --full --trials 3")
    else:
        print(f"  syscity eval run {file_stem} --full")
    return 0


if __name__ == "__main__":
    sys.exit(main())
