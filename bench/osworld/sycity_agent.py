"""An OSWorld agent that re-drives syscity's own run on OSWorld's step loop.

OSWorld asks an agent for the *next actions* and executes them itself
(`env.step`). syscity is a whole agent: it owns its observe/act loop over the
VM's HTTP desktop service. The two are joined by replay:

1. The first `predict` for a task runs `syscity bench run` once against the same
   VM. syscity looks, decides and acts; its result file records every pyautogui
   snippet it sent in the `actions` field.
2. Every later `predict` hands one snippet back, and OSWorld's own `env.step`
   executes it. When the queue is empty the agent reports `DONE` — or `FAIL` if
   syscity stopped without an answer (budget exhausted or an agent-side error).

What replay cannot reproduce is interleaving. syscity decided step N+1 from the
screen it had built up itself, not from a fresh observation taken after
OSWorld's pause; a task whose next step depends on a load finishing between
steps is timed by `--sleep-after-execution`, not by syscity's own waiting. The
step protocol in docs/computer-use-benchmark.md §4.4 removes this, and this
file is where it will land.

Nothing here writes a score. The evaluator is OSWorld's, and syscity never sees
it.
"""

from __future__ import annotations

import json
import logging
import os
import subprocess
import tempfile

logger = logging.getLogger("desktopenv.syscity")


class SyscityUnavailable(RuntimeError):
    """The syscity binary could not be started at all."""


class SyscityRunFailed(RuntimeError):
    """syscity ran but produced no usable result.

    This is an infrastructure failure, not a task the model got wrong: it goes
    to the harness's `record_example_error` path and stays out of the success
    rate's denominator. Raising instead of returning a `FAIL` action matters —
    for an infeasible task, `FAIL` scores 1.
    """


def _describe(snippet):
    """The last meaningful line of a pyautogui snippet, for the trajectory log."""
    lines = [line.strip() for line in snippet.strip().splitlines() if line.strip()]
    for line in reversed(lines):
        if not line.startswith("import "):
            return line
    return lines[0] if lines else ""


class SyscityAgent:
    """OSWorld's duck-typed agent interface, backed by one `syscity bench run`.

    OSWorld agents need `predict(instruction, obs) -> (response, actions)` and
    `reset(_logger=None, vm_ip=None, **kwargs)`; nothing else is required.
    """

    def __init__(
        self,
        binary="syscity",
        max_steps=60,
        provider=None,
        model=None,
        base_url=None,
        port=5000,
        timeout=1800.0,
        artifacts_dir=None,
        extra_args=None,
        env=None,
    ):
        self.binary = binary
        self.max_steps = max_steps
        self.provider = provider
        self.model = model
        self.base_url = base_url
        self.port = port
        self.timeout = timeout
        # Where the result file for the current task goes. run.py sets this to
        # the harness's own per-task directory so both transcripts sit together.
        self.artifacts_dir = artifacts_dir
        self.extra_args = list(extra_args or [])
        self.env = env

        self.task_id = "osworld"
        self.vm_ip = None
        self.last_result = None
        self._queue = []
        self._terminal = "DONE"

    # ── OSWorld's agent interface ───────────────────────────────────────────

    def reset(self, _logger=None, vm_ip=None, **kwargs):
        global logger
        if _logger is not None:
            logger = _logger
        self.vm_ip = vm_ip
        self.last_result = None
        self._queue = []
        self._terminal = "DONE"

    def predict(self, instruction, obs):
        if self.last_result is None:
            self.last_result = self._run(instruction)
            self._queue = list(self.last_result.get("actions") or [])
            stopped = self.last_result.get("stop_reason")
            self._terminal = "DONE" if stopped == "final_answer" else "FAIL"
            logger.info(
                "syscity finished %s: %s, %d action(s)",
                self.task_id,
                stopped,
                len(self._queue),
            )

        if self._queue:
            snippet = self._queue.pop(0)
            # The harness stores `response` next to the action in traj.jsonl;
            # name the operation, not the `import` line every snippet starts with.
            return "replaying: {0}".format(_describe(snippet)), [snippet]

        return "syscity is done", [self._terminal]

    # ── One syscity run per task ────────────────────────────────────────────

    def _run(self, instruction):
        if not self.vm_ip:
            raise SyscityRunFailed(
                "no VM address: the harness must call reset(vm_ip=...) before predict()"
            )

        out_dir = self._out_dir()
        result_path = os.path.join(out_dir, "result.json")
        if os.path.exists(result_path):
            os.remove(result_path)

        command = [
            self.binary,
            "bench",
            "run",
            "--instruction",
            instruction,
            "--computer-url",
            "http://{0}:{1}".format(self.vm_ip, self.port),
            "--task-id",
            self.task_id,
            "--max-steps",
            str(self.max_steps),
            "--out",
            result_path,
        ]
        for flag, value in (
            ("--provider", self.provider),
            ("--model", self.model),
            ("--base-url", self.base_url),
        ):
            if value:
                command.extend([flag, value])
        command.extend(self.extra_args)

        logger.info("running %s", " ".join(command))
        try:
            completed = subprocess.run(
                command,
                capture_output=True,
                text=True,
                timeout=self.timeout,
                env=self._subprocess_env(),
            )
        except FileNotFoundError as error:
            raise SyscityUnavailable(
                "could not start {0!r}: build it with `cargo build --release` or "
                "pass --syscity".format(self.binary)
            ) from error
        except subprocess.TimeoutExpired as error:
            raise SyscityRunFailed(
                "{0} did not finish within {1:.0f}s".format(self.task_id, self.timeout)
            ) from error

        if completed.stdout:
            logger.debug("syscity stdout:\n%s", completed.stdout)
        if completed.returncode != 0:
            raise SyscityRunFailed(
                "syscity exited {0} for {1}: {2}".format(
                    completed.returncode,
                    self.task_id,
                    (completed.stderr or completed.stdout or "").strip()[-2000:],
                )
            )

        try:
            with open(result_path, "r", encoding="utf-8") as handle:
                result = json.load(handle)
        except (OSError, ValueError) as error:
            raise SyscityRunFailed(
                "no readable result for {0} at {1}: {2}".format(
                    self.task_id, result_path, error
                )
            ) from error

        if result.get("error"):
            # The agent could not run (provider failure, transport). The model
            # never got a turn, so this is not a wrong answer.
            raise SyscityRunFailed(
                "syscity reported an agent error for {0}: {1}".format(
                    self.task_id, result["error"]
                )
            )

        logger.info("answer: %s", (result.get("answer") or "").strip()[:2000])
        return result

    def _out_dir(self):
        if self.artifacts_dir:
            os.makedirs(self.artifacts_dir, exist_ok=True)
            return self.artifacts_dir
        # Without a harness directory, keep the file somewhere it can be read
        # afterwards rather than in an anonymous temp dir that vanishes.
        return tempfile.mkdtemp(prefix="syscity-{0}-".format(self.task_id))

    def _subprocess_env(self):
        env = dict(os.environ)
        if self.env:
            env.update(self.env)
        # A budget passed through the environment would silently win over
        # --max-steps on some codepaths; the flag is the single source here.
        env.pop("SYSCITY_MAX_TOOL_ITERATIONS", None)
        return env


def summarize(result_paths, scores, infra_errors):
    """The report run.py prints and writes. Kept here so tests can reach it."""
    scored = len(scores)
    passed = sum(1 for score in scores if score and score > 0)
    return {
        "schema": 1,
        "scored": scored,
        "passed": passed,
        "success_rate": (passed / scored) if scored else None,
        "infra_errors": len(infra_errors),
        "infra_error_examples": infra_errors,
        "result_paths": result_paths,
    }
