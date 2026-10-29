"""OSWorld agents backed by syscity, in either of its two drive shapes.

OSWorld asks an agent for the *next actions* and executes them itself
(`env.step`). syscity is a whole agent: it owns its observe/act loop over the
VM's HTTP desktop service. The two are joined in one of two ways.

**Replay** (`SyscityAgent`). The first `predict` for a task runs
`syscity bench run` once against the same VM. syscity looks, decides and acts;
its result file records every pyautogui snippet it sent in the `actions` field.
Every later `predict` hands one snippet back and OSWorld's own `env.step`
executes it. When the queue is empty the agent reports `DONE` — or `FAIL` if
syscity stopped without an answer.

**Step** (`SyscityStepAgent`). The first `predict` starts `syscity bench serve`
and sends it the task; syscity then sends one snippet per turn and blocks until
the harness has executed it and taken its own observation. That is the same
ordering OSWorld's loop has, and it is what replay cannot reproduce: under
replay, syscity decided step N+1 from the screen it had built up itself, not
from a fresh observation taken after OSWorld's pause.

Both are multi-process one-task-at-a-time designs, because one VM is restored
per task and element ids are addresses inside that state only.

Nothing here writes a score. The evaluator is OSWorld's, and syscity never sees
it.
"""

from __future__ import annotations

import json
import logging
import os
import queue
import subprocess
import tempfile
import threading
import time

logger = logging.getLogger("desktopenv.syscity")

# Must match `SERVE_SCHEMA` in src/bench/serve.rs.
SERVE_SCHEMA = 1


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
        self._dir = None

    # ── OSWorld's agent interface ───────────────────────────────────────────

    def reset(self, _logger=None, vm_ip=None, **kwargs):
        global logger
        if _logger is not None:
            logger = _logger
        self.vm_ip = vm_ip
        self.last_result = None
        self._queue = []
        self._terminal = "DONE"
        self._dir = None

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
        # Cached so the stderr log and the result file of one task land
        # together even when no harness directory was given.
        if self._dir is None:
            if self.artifacts_dir:
                os.makedirs(self.artifacts_dir, exist_ok=True)
                self._dir = self.artifacts_dir
            else:
                # Without a harness directory, keep the file somewhere it can
                # be read afterwards rather than in a temp dir that vanishes.
                self._dir = tempfile.mkdtemp(prefix="syscity-{0}-".format(self.task_id))
        return self._dir

    def _subprocess_env(self):
        env = dict(os.environ)
        if self.env:
            env.update(self.env)
        # A budget passed through the environment would silently win over
        # --max-steps on some codepaths; the flag is the single source here.
        env.pop("SYSCITY_MAX_TOOL_ITERATIONS", None)
        return env


class _ServeChannel:
    """One `syscity bench serve` child, spoken to a line at a time.

    Reads happen on a thread because a blocking `readline` cannot be given a
    deadline, and a benchmark that hangs is worse than one that errors: the
    VM stays held and every later task pays for it.
    """

    def __init__(self, command, env, stderr_path):
        try:
            self._stderr = open(stderr_path, "wb")
        except OSError:
            self._stderr = subprocess.DEVNULL
        try:
            self._process = subprocess.Popen(
                command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=self._stderr,
                text=True,
                bufsize=1,
                env=env,
            )
        except FileNotFoundError as error:
            self._close_stderr()
            raise SyscityUnavailable(
                "could not start {0!r}: build it with `cargo build --release` or "
                "pass --syscity".format(command[0])
            ) from error
        except OSError as error:
            self._close_stderr()
            raise SyscityRunFailed(
                "could not start {0!r}: {1}".format(command[0], error)
            ) from error

        self._lines = queue.Queue()
        self._reader = threading.Thread(target=self._pump, daemon=True)
        self._reader.start()

    def _pump(self):
        try:
            for line in self._process.stdout:
                self._lines.put(line)
        finally:
            # A closed pipe and a dead process are the same thing to the
            # reader: nothing more is coming.
            self._lines.put(None)

    def _close_stderr(self):
        if self._stderr is not subprocess.DEVNULL:
            try:
                self._stderr.close()
            except OSError:
                pass

    def send(self, payload):
        line = json.dumps(payload) + "\n"
        try:
            self._process.stdin.write(line)
            self._process.stdin.flush()
        except (BrokenPipeError, ValueError) as error:
            # ValueError: the pipe was already closed by `close`.
            raise SyscityRunFailed(
                "syscity bench serve is gone; could not send {0}: {1}".format(
                    payload.get("type"), error
                )
            ) from error

    def read(self, timeout):
        try:
            line = self._lines.get(timeout=max(0.0, timeout))
        except queue.Empty:
            raise SyscityRunFailed(
                "syscity bench serve said nothing for {0:.0f}s".format(timeout)
            ) from None
        if line is None:
            code = self._process.poll()
            raise SyscityRunFailed(
                "syscity bench serve exited ({0}) without answering".format(code)
            )
        try:
            message = json.loads(line)
        except ValueError as error:
            raise SyscityRunFailed(
                "unreadable line from syscity: {0!r}".format(line.strip()[:500])
            ) from error
        if not isinstance(message, dict):
            raise SyscityRunFailed(
                "unexpected line from syscity: {0!r}".format(line.strip()[:500])
            )
        return message

    def close(self, timeout=10.0):
        """Let the child finish, then insist."""
        if self._process.poll() is None:
            try:
                self._process.stdin.close()
            except (OSError, ValueError):
                pass
        try:
            self._process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            logger.warning("syscity bench serve did not exit; terminating it")
            self._process.terminate()
            try:
                self._process.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                self._process.kill()
                self._process.wait()
        try:
            self._process.stdout.close()
        except (OSError, ValueError):
            pass
        self._close_stderr()


class SyscityStepAgent:
    """OSWorld's agent interface, backed by `syscity bench serve` over stdio.

    One syscity process per task, and one action per `predict`. Between the two
    the child is blocked waiting for the harness's `result` line, so syscity
    cannot look at the screen again until OSWorld has stepped and observed —
    which is the ordering the benchmark is measuring against.

    The line vocabulary is `bench/serve.rs`'s:
    `ready` → `task` → (`action` ⇄ `result`)* → `done` | `error`.
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
        self.artifacts_dir = artifacts_dir
        self.extra_args = list(extra_args or [])
        self.env = env

        self.task_id = "osworld"
        self.vm_ip = None
        self.last_result = None
        self._channel = None
        self._deadline = None
        # True while the harness holds an action of ours it has not reported
        # back on; syscity is blocked in `deliver` until we clear it.
        self._awaiting_result = False
        self._terminal = None
        self._dir = None

    # ── OSWorld's agent interface ───────────────────────────────────────────

    def reset(self, _logger=None, vm_ip=None, **kwargs):
        global logger
        if _logger is not None:
            logger = _logger
        self.close()
        self.vm_ip = vm_ip
        self.last_result = None
        self._deadline = None
        self._awaiting_result = False
        self._terminal = None
        self._dir = None

    def predict(self, instruction, obs):
        if self._terminal is not None:
            return "syscity is done", [self._terminal]
        if self._channel is None:
            self._start(instruction)
        elif self._awaiting_result:
            # The harness executed the last action and is asking for the next
            # one; only now may syscity read the screen again. `ok` is always
            # true because OSWorld's agent interface has no per-action status:
            # a `step` that raised never reaches this call, so being asked
            # again is the only success signal there is.
            self._channel.send({"type": "result", "ok": True, "output": ""})
            self._awaiting_result = False
        return self._next()

    def close(self):
        """Release the child. `run.py` calls this on every exit path."""
        if self._channel is not None:
            channel, self._channel = self._channel, None
            channel.close()

    # ── The serve protocol ──────────────────────────────────────────────────

    def _start(self, instruction):
        if not self.vm_ip:
            raise SyscityRunFailed(
                "no VM address: the harness must call reset(vm_ip=...) before predict()"
            )

        command = [
            self.binary,
            "bench",
            "serve",
            "--computer-url",
            "http://{0}:{1}".format(self.vm_ip, self.port),
        ]
        if self.max_steps:
            command.extend(["--max-steps", str(self.max_steps)])
        for flag, value in (
            ("--provider", self.provider),
            ("--model", self.model),
            ("--base-url", self.base_url),
        ):
            if value:
                command.extend([flag, value])
        command.extend(self.extra_args)

        stderr_path = os.path.join(self._out_dir(), "serve.stderr.log")
        logger.info("stepping %s through %s", self.task_id, " ".join(command))
        self._channel = _ServeChannel(command, self._subprocess_env(), stderr_path)
        # One deadline for the whole task: the harness's own per-task timeout
        # is outside this process, so the wait itself has to be bounded here.
        self._deadline = time.monotonic() + self.timeout

        greeting = self._channel.read(self._remaining())
        if greeting.get("type") != "ready":
            raise SyscityRunFailed(
                "syscity bench serve said {0!r} instead of ready".format(greeting)
            )
        schema = greeting.get("schema")
        if schema is not None and schema != SERVE_SCHEMA:
            raise SyscityRunFailed(
                "syscity speaks schema {0}, this agent speaks {1}".format(
                    schema, SERVE_SCHEMA
                )
            )

        self._channel.send(
            {"type": "task", "task_id": self.task_id, "instruction": instruction}
        )

    def _next(self):
        while True:
            message = self._channel.read(self._remaining())
            kind = message.get("type")
            if kind == "action":
                code = message.get("code") or ""
                if not code.strip():
                    raise SyscityRunFailed("syscity sent an empty action")
                self._awaiting_result = True
                # Name the operation, not the `import` line every snippet
                # starts with: the harness stores this next to the action.
                return "stepping: {0}".format(_describe(code)), [code]
            if kind == "done":
                return self._finish(message)
            if kind == "error":
                raise SyscityRunFailed(
                    "syscity reported an error for {0}: {1}".format(
                        self.task_id, message.get("error")
                    )
                )
            raise SyscityRunFailed(
                "unexpected line from syscity: {0!r}".format(message)
            )

    def _finish(self, message):
        result = message.get("result") or {}
        schema = message.get("schema")
        if schema is not None and schema != SERVE_SCHEMA:
            logger.warning(
                "syscity reported schema %s, this agent speaks %s", schema, SERVE_SCHEMA
            )

        self.last_result = result
        self._write_result(result)
        # The child has said its last word; let it exit before the harness
        # moves on to the next VM.
        self.close()

        if result.get("error"):
            # The model never got a turn (or the turn failed); not a wrong
            # answer, so it must not become a scored `FAIL`.
            raise SyscityRunFailed(
                "syscity reported an agent error for {0}: {1}".format(
                    self.task_id, result["error"]
                )
            )
        stopped = result.get("stop_reason")
        self._terminal = "DONE" if stopped == "final_answer" else "FAIL"
        logger.info("syscity finished %s: %s", self.task_id, stopped)
        logger.info("answer: %s", (result.get("answer") or "").strip()[:2000])
        return "syscity is done", [self._terminal]

    def _remaining(self):
        if self._deadline is None:
            return self.timeout
        left = self._deadline - time.monotonic()
        if left <= 0:
            self.close()
            raise SyscityRunFailed(
                "{0} did not finish within {1:.0f}s".format(self.task_id, self.timeout)
            )
        return left

    # ── Files and environment ───────────────────────────────────────────────

    def _write_result(self, result):
        path = os.path.join(self._out_dir(), "result.json")
        try:
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(result, handle, indent=2)
                handle.write("\n")
        except (OSError, TypeError) as error:
            # The trajectory is already in the harness's hands; losing this
            # copy must not fail the task.
            logger.warning("could not write %s: %s", path, error)

    def _out_dir(self):
        if self._dir is None:
            if self.artifacts_dir:
                os.makedirs(self.artifacts_dir, exist_ok=True)
                self._dir = self.artifacts_dir
            else:
                self._dir = tempfile.mkdtemp(prefix="syscity-{0}-".format(self.task_id))
        return self._dir

    def _subprocess_env(self):
        env = dict(os.environ)
        if self.env:
            env.update(self.env)
        # The flag is the single source of the budget; an inherited value would
        # silently win on some codepaths.
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
