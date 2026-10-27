#!/usr/bin/env python3
"""Score one GAIA trial with the official scorer.

This is the `exit_code` condition behind every `gaia_l1_*` task: the harness
runs it against the agent's final message and the trial passes only when it
exits 0.

    python3 evals/importers/gaia_score.py --task <uuid> \
        --answer ${trial_dir}/response.txt

Exit codes (the mapping `GoalCondition::ExitCode` expects):

    0  the official `question_scorer` accepted the answer
    1  it rejected the answer — a real miss
    2  the trial could not be scored (bad args, unknown task, unreadable
       answer file). Kept distinct from 1 so a broken harness is not counted
       as a wrong answer; both fail the trial.

The scorer itself is `gaia_scorer.py`, vendored from the GAIA leaderboard
Space. This file adds no scoring logic of its own. The one thing it does add is
*answer extraction*: the agent's message is free-form, while the scorer expects
just the answer. If the message contains a `FINAL ANSWER:` line (every task
prompt asks for one) the text after the marker is scored. If it does not, the
last non-empty line is tried first and the whole message second — the same
order a human reader would use, and both candidates go through the untouched
official scorer. Extraction is uniform across tasks, and the raw message stays
in the trial directory for audit.

Answers live in `evals/capability/gaia_l1_answers.json` (written by `gaia.py`,
gitignored along with the generated task file). `--answers` overrides it.
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import re
import sys
from pathlib import Path

from gaia_scorer import question_scorer

# `FINAL ANSWER: x` — the marker the generated task prompts ask for. The value
# continues until a blank line, so a wrapped answer still scores.
_FINAL_ANSWER = re.compile(r"(?im)^[ \t>*#]*FINAL ANSWER:[ \t]*(.*)$")

# Lead-ins a reply may put in front of a bare answer when it forgot the marker.
_ANSWER_LEAD_IN = re.compile(
    r"(?i)^(?:the\s+)?(?:final\s+)?answer\s*(?:is|:)\s*|^答案\s*(?:是|为)?\s*[:：]?\s*"
)


def extract_answer(response: str) -> str:
    """Return what the official scorer should see.

    Prefers the last `FINAL ANSWER:` marker (an agent may restate its plan
    earlier in the message); falls back to the untrimmed message.
    """
    matches = list(_FINAL_ANSWER.finditer(response))
    if not matches:
        return response.strip()

    last = matches[-1]
    tail = response[last.end() :]
    # The marker's own line ends at the newline; the answer itself may continue
    # on the lines below until a blank one.
    if tail.startswith("\n"):
        tail = tail[1:]
    continuation = []
    for line in tail.splitlines():
        if not line.strip():
            break
        continuation.append(line.strip())

    answer = " ".join([last.group(1).strip(), *continuation]).strip()
    # Strip the wrappers models habitually add around citations of an answer.
    answer = answer.strip("`*_ ")
    if len(answer) >= 2 and answer[0] == answer[-1] and answer[0] in "\"'":
        answer = answer[1:-1]
    return answer.strip()


def fallback_candidates(response: str) -> list[str]:
    """Candidates to try when the reply carries no `FINAL ANSWER:` marker.

    Ordered most-specific first: the last non-empty line (what a human would
    read as the answer), then the whole message (the pre-existing behaviour).
    The last line is only offered when it is short enough to *be* an answer —
    a concluding paragraph is not a candidate.
    """
    lines = [line.strip() for line in response.splitlines() if line.strip()]
    candidates = []
    if lines:
        last = lines[-1].lstrip(">*#- ").strip()
        if 0 < len(last) <= 300:
            candidates.append(last)
            # "The answer is X." / "答案：X" — the official scorer compares the
            # normalized string exactly, so a lead-in would otherwise ensure a
            # miss even though the answer is right there.
            stripped = _ANSWER_LEAD_IN.sub("", last).strip("`*_ .。")
            if stripped and stripped != last:
                candidates.append(stripped)
    whole = response.strip()
    if whole and whole not in candidates:
        candidates.append(whole)
    return candidates


def default_answers_path() -> Path:
    return Path(__file__).resolve().parent.parent / "capability" / "gaia_l1_answers.json"


def load_answers(path: Path) -> dict:
    with path.open(encoding="utf-8") as f:
        return json.load(f)


def score(task_id: str, answer_text: str, answers: dict, *, show_expected: bool) -> int:
    record = answers.get(task_id)
    if record is None:
        print(f"GAIA task {task_id}: ERROR — no ground truth in the answers file")
        return 2
    expected = record["answer"]
    if _FINAL_ANSWER.search(answer_text):
        candidates = [extract_answer(answer_text)]
    else:
        candidates = fallback_candidates(answer_text)

    scored: list[tuple[str, bool]] = []
    for candidate in candidates:
        # The official scorer narrates on stdout; keep this command's output to
        # a single line so the harness log stays readable.
        with contextlib.redirect_stdout(io.StringIO()):
            scored.append((candidate, bool(question_scorer(candidate, expected))))
    hit = any(passed for _, passed in scored)
    got = next((text for text, passed in scored if passed), candidates[0] if candidates else "")

    verdict = "PASS" if hit else "FAIL"
    print(f"GAIA task {task_id}: {verdict}")
    print(f"  scored: {got!r}"[:2000])
    if len(scored) > 1 and not hit:
        print(f"  also tried: {scored[-1][0]!r}"[:2000])
    if show_expected:
        print(f"  expected: {expected!r}")
    return 0 if hit else 1


def self_test() -> int:
    """Check the vendored scorer and the extraction against known pairs.

    Runs without GAIA data so CI can prove the plumbing before someone has
    download access.
    """
    cases = [
        ("FINAL ANSWER: Paris", "Paris", True),
        ("FINAL ANSWER: paris.", "Paris", True),
        ("After checking, the answer is Madrid.", "Paris", False),
        ("FINAL ANSWER: 1,234", "1234", True),
        ("FINAL ANSWER: apple; banana", "apple, banana", True),
        ("FINAL ANSWER: apple", "apple, banana", False),
        ("FINAL ANSWER: 3", "4", False),
    ]
    failures = 0
    for response, expected, want in cases:
        got = extract_answer(response)
        with contextlib.redirect_stdout(io.StringIO()):
            hit = question_scorer(got, expected)
        ok = hit == want
        failures += 0 if ok else 1
        print(f"{'ok  ' if ok else 'FAIL'} {response!r} vs {expected!r} -> {hit}")

    # A wrapped answer must survive the blank-line rule.
    wrapped = extract_answer("work\nFINAL ANSWER: New York City\n(state, not the state)")
    if wrapped != "New York City (state, not the state)":
        print(f"FAIL wrapped extraction -> {wrapped!r}")
        failures += 1

    # Without the marker, the last line is tried before the whole message.
    no_marker = fallback_candidates("I checked several sources.\nThe answer is **Madrid**.")
    if not no_marker or no_marker[0] != "The answer is **Madrid**.":
        print(f"FAIL fallback candidates -> {no_marker!r}")
        failures += 1
    cleaned = [c for c in no_marker if c == "Madrid"]
    if not cleaned:
        print(f"FAIL lead-in strip -> {no_marker!r}")
        failures += 1
    else:
        with contextlib.redirect_stdout(io.StringIO()):
            if not question_scorer(cleaned[0], "Madrid"):
                print("FAIL fallback scoring")
                failures += 1

    total = len(cases) + 3
    print(f"{total - failures}/{total} self-tests passed")
    return 0 if failures == 0 else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--task", help="GAIA task_id to score against")
    parser.add_argument("--answer", help="file holding the agent's final message")
    parser.add_argument(
        "--answers",
        type=Path,
        default=default_answers_path(),
        help="ground-truth JSON written by gaia.py (default: %(default)s)",
    )
    parser.add_argument(
        "--show-expected",
        action="store_true",
        help="print the ground truth (off by default so shared logs stay clean)",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="check the vendored scorer and extraction, then exit",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    if not args.task or not args.answer:
        parser.error("--task and --answer are required unless --self-test is given")

    try:
        response = Path(args.answer).read_text(encoding="utf-8", errors="replace")
    except OSError as e:
        print(f"GAIA task {args.task}: ERROR — cannot read answer: {e}")
        return 2

    try:
        answers = load_answers(args.answers)
    except (OSError, json.JSONDecodeError) as e:
        print(f"GAIA task {args.task}: ERROR — cannot read {args.answers}: {e}")
        return 2

    return score(args.task, response, answers, show_expected=args.show_expected)


if __name__ == "__main__":
    sys.exit(main())
