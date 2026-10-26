#!/usr/bin/env python3
"""Import GAIA validation Level 1 into the local eval harness.

Produces two files, both gitignored because the GAIA terms of use do not allow
redistribution:

    evals/capability/gaia_l1.yaml           tasks, one per question
    evals/capability/gaia_l1_answers.json   ground truth, keyed by task_id

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
    ~/.cache/syscity-bench/gaia/venv/bin/pip install pyarrow
    ~/.cache/syscity-bench/gaia/venv/bin/python evals/importers/gaia.py

The token is read from `--token`, `$HF_TOKEN`, or `~/.cache/huggingface/token`
(in that order); `$HTTPS_PROXY` is honoured, as the HF endpoints are often
unreachable otherwise.

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
METADATA_PATH = "2023/validation/metadata.level1.parquet"
EXPECTED_LEVEL = 1
# The validation split has 53 Level-1 questions; a big deviation means the
# dataset moved under us and the ledger numbers would not be comparable.
EXPECTED_COUNT = 53

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
        return explicit
    env = os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN")
    if env:
        return env.strip()
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
) -> tuple[list[dict], dict]:
    tasks: list[dict] = []
    answers: dict = {}
    seen: set[str] = set()
    scorer = (REPO_ROOT / "evals" / "importers" / "gaia_score.py").resolve()

    level1 = [r for r in rows if int(r["Level"]) == EXPECTED_LEVEL]
    level1.sort(key=lambda r: r["task_id"])
    if limit is not None:
        level1 = level1[:limit]

    for row in level1:
        task_id = row["task_id"]
        slug = f"gaia_l1_{task_id[:8]}"
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

        tasks.append(
            {
                "id": slug,
                "description": f"GAIA validation Level 1 · task_id={task_id}",
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
                            f"python3 {scorer} --task {task_id} "
                            "--answer ${trial_dir}/response.txt"
                        ),
                        "expected": 0,
                    }
                ],
            }
        )
        answers[task_id] = {
            "id": slug,
            "level": EXPECTED_LEVEL,
            "answer": expected,
            "has_attachment": bool(remote),
        }

    return tasks, answers


YAML_HEADER = """\
# GAIA Level 1 (validation) — GENERATED FILE, DO NOT EDIT.
#
# Regenerate with:
#   ~/.cache/syscity-bench/gaia/venv/bin/python evals/importers/gaia.py
#
# Not committed: the GAIA terms of use forbid redistribution, and the
# attachment paths are machine-specific. Ground truth lives beside this file in
# `gaia_l1_answers.json`; scoring is the official GAIA `question_scorer`
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
            load_rows(parquet), tmpdir / "files", None, download_files=False, limit=None
        )
        assert len(tasks) == 2, tasks
        assert [t["id"] for t in tasks] == ["gaia_l1_00000000", "gaia_l1_bbbbbbbb"]

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
        default=REPO_ROOT / "evals" / "capability" / "gaia_l1.yaml",
        help="generated task file (default: %(default)s)",
    )
    parser.add_argument(
        "--answers-out",
        type=Path,
        default=REPO_ROOT / "evals" / "capability" / "gaia_l1_answers.json",
        help="ground-truth JSON (default: %(default)s)",
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
        help=f"do not fail when the row count differs from {EXPECTED_COUNT}",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="exercise the importer on synthetic rows and exit",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    cache = cache_root(args.cache)
    metadata = args.metadata or cache / "metadata" / "level1.parquet"
    token = read_token(args.token)
    if token is None:
        print("warning: no HF token found; gated downloads will fail", file=sys.stderr)

    if not metadata.exists():
        print(f"downloading {METADATA_PATH} -> {metadata}")
        fetch(HF_BASE + METADATA_PATH, metadata, token)

    rows = load_rows(metadata)
    tasks, answers = build_tasks(
        rows,
        cache / "files",
        token,
        download_files=not args.no_files,
        limit=args.limit,
    )
    if (
        args.limit is None
        and len(tasks) != EXPECTED_COUNT
        and not args.allow_count_drift
    ):
        raise SystemExit(
            f"expected {EXPECTED_COUNT} Level-1 tasks, built {len(tasks)}. "
            "The dataset may have been updated — re-check before comparing "
            "against the ledger, then pass --allow-count-drift to proceed."
        )

    write_yaml(args.out, tasks)
    args.answers_out.parent.mkdir(parents=True, exist_ok=True)
    args.answers_out.write_text(
        json.dumps(answers, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )

    with_files = sum(1 for r in answers.values() if r["has_attachment"])
    print(f"wrote {len(tasks)} tasks -> {args.out}")
    print(f"  {with_files} with attachments, cached under {cache / 'files'}")
    print(f"wrote ground truth -> {args.answers_out}")
    print()
    print("Run the baseline with:")
    print("  syscity eval run gaia_l1 --full --trials 3")
    return 0


if __name__ == "__main__":
    sys.exit(main())
