"""The Cua Bench agent, backed by one `syscity bench run` per task.

Cua Bench's own agents decide and act one step at a time (`perform_task`
receives a live `DesktopSession` and calls `session.execute_action`). syscity is
a whole agent: it owns its observe/act loop. So syscity runs once against the
task's desktop and acts on it directly, and this file's job is the join — start
the desktop it can reach, run it, and report what happened. Cua Bench's own
evaluator decides the score; nothing here does.

## Why there is a bridge

syscity's remote-desktop client speaks the OSWorld VM protocol —
`GET /screenshot`, `GET /accessibility`, `POST /screen_size`,
`POST /execute` (see `src/computer/remote/osworld.rs`). A Cua Bench session is a
`cua_sandbox.Sandbox` with a shell, mouse and screen behind the SDK.

Rather than teach syscity a second protocol, this module starts `cua_bridge.py`
**inside the sandbox** and tunnels its port to the host. That bridge answers the
four endpoints from the guest itself:

* `/screenshot` captures the display with ImageMagick, from the same X server
  `session.screenshot()` reads — so the frame syscity reasons about is the frame
  the evaluator will see;
* `/accessibility` renders the guest's AT-SPI tree (`atspi_xml.py`);
* `/execute` runs the snippet in the guest with `pyautogui` and `pyperclip`
  bound to xdotool and xclip (`pyautogui_shim.py`), because the bench image
  ships neither.

This is deliberately the *direct* shape, not a replay: syscity's action really
lands when it is taken, the same way it does against an OSWorld VM. Replaying
recorded snippets through `session.execute_action` would be the fallback if the
guest could not execute them, and it would be a weaker measurement — the screen
syscity acted from would no longer be the screen the action changed.

## What is and is not this file's job

* A failure to start syscity, a run that fails, or a result carrying an `error`
  is raised as `SyscityRunFailed`, so the harness records an infrastructure
  error and never a wrong answer. A broken harness and a bad model are different
  facts and only one of them belongs in a success rate.
* A model that finishes without solving — `stop_reason` of `max_steps` — is a
  real `FailureMode.MAX_STEPS_EXCEEDED`. That one *is* a model outcome and does
  belong in the denominator.
* The snippets syscity sent are logged and attached to the trace. They are the
  trajectory; this file does not run them a second time.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import re
import subprocess
import sys
import tempfile

from cua_bench.agents import BaseAgent, FailureMode, register_agent
from cua_bench.agents.base import AgentResult

logger = logging.getLogger("cua_bench.syscity")

#: The guest port the bridge listens on. Anything syscity can reach would do;
#: this is also the OSWorld VM service's port, so the `--computer-url` shape is
#: identical.
BRIDGE_PORT = 8765

#: Files the bridge needs inside the guest. They live beside this module.
BRIDGE_FILES = ("cua_bridge.py", "atspi_xml.py", "pyautogui_shim.py")

#: Where those files and the bridge's log go inside the guest.
GUEST_DIR = "/tmp/syscity-cua"

HERE = os.path.dirname(os.path.abspath(__file__))

_IMPORT_LINE = re.compile(r"^\s*(?:import|from)\s+\w+")


class SyscityUnavailable(RuntimeError):
    """The syscity binary could not be started at all."""


class SyscityRunFailed(RuntimeError):
    """syscity ran but produced no usable result.

    An infrastructure failure, not a task the model got wrong — which is why it
    is raised rather than reported as a `FailureMode`. A harness bug must not
    look like a model failure.
    """


# ── Reading syscity's result ───────────────────────────────────────────────


def describe(snippet: str) -> str:
    """The last meaningful line of a snippet, for logs and the trace.

    Naming the operation (`pyautogui.click(525, 309)`) rather than the
    `import pyautogui` every snippet starts with is what makes a trajectory
    readable.
    """
    lines = [line.strip() for line in snippet.strip().splitlines() if line.strip()]
    for line in reversed(lines):
        if not _IMPORT_LINE.match(line):
            return line
    return lines[0] if lines else ""


def snippets_from_result(result: dict) -> list[str]:
    """The action snippets syscity recorded, oldest first.

    `actions` is optional: a result from before the field existed, or a run
    that stopped before touching the desktop, legitimately has none.
    """
    raw = result.get("actions")
    if not raw:
        return []
    if not isinstance(raw, list) or not all(isinstance(item, str) for item in raw):
        raise SyscityRunFailed(
            "syscity's result has an `actions` field of the wrong shape ({0})".format(
                type(raw).__name__
            )
        )
    return [item for item in raw if item.strip()]


# ── The guest bridge ───────────────────────────────────────────────────────


async def start_bridge(sandbox):
    """Copy the bridge into the sandbox, start it, and return `(url, tunnel)`.

    The tunnel must outlive the run, so it is handed back for the caller to
    close; syscity reaches the bridge through it from the host.
    """
    for name in BRIDGE_FILES:
        source = os.path.join(HERE, name)
        if not os.path.isfile(source):
            raise SyscityRunFailed(
                "the Cua bridge is incomplete: {0} is missing".format(source)
            )
        await sandbox.files.upload(source, "{0}/{1}".format(GUEST_DIR, name))

    # Detached, because the bridge has to outlive the shell command that starts
    # it; its output stays readable when it fails to come up.
    start = (
        "mkdir -p {dir} && cd {dir} && "
        "nohup python3 {dir}/cua_bridge.py > {dir}/bridge.log 2>&1 & echo $!"
    ).format(dir=GUEST_DIR)
    started = await sandbox.shell.run(start)
    if started.returncode != 0:
        raise SyscityRunFailed(
            "could not start the Cua bridge in the sandbox: {0}".format(
                (started.stderr or started.stdout or "").strip()[:2000]
            )
        )

    tunnel = await sandbox.tunnel.forward(BRIDGE_PORT).__aenter__()
    url = "http://{0}:{1}".format(tunnel.host, tunnel.port)
    await wait_for_bridge(sandbox, url)
    return url, tunnel


async def wait_for_bridge(sandbox, url, timeout=30.0):
    """Block until the guest bridge answers, or say why it did not."""
    import urllib.request

    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    last = "no attempt made"
    while loop.time() < deadline:
        try:
            with urllib.request.urlopen(url + "/health", timeout=2.0) as response:
                if response.status == 200:
                    return
                last = "HTTP {0}".format(response.status)
        except Exception as error:  # noqa: BLE001 - retried until the deadline
            last = str(error)
        await asyncio.sleep(0.25)

    log = await sandbox.shell.run(
        "cat {0}/bridge.log 2>/dev/null | tail -c 2000".format(GUEST_DIR)
    )
    raise SyscityRunFailed(
        "the Cua bridge did not answer at {0} within {1:.0f}s ({2}). "
        "Its log:\n{3}".format(url, timeout, last, (log.stdout or "").strip() or "(empty)")
    )


# ── The agent ──────────────────────────────────────────────────────────────


class SyscityAgent(BaseAgent):
    """One `syscity bench run` per task, acting on the sandbox as it goes.

    Keywords beyond the ones `cb run` passes through:

    * ``binary`` — the syscity executable (default: ``$SYSCITY_BIN``, else
      ``syscity`` on PATH);
    * ``max_steps`` — syscity's tool-iteration budget;
    * ``provider`` / ``model`` / ``base_url`` — forwarded to `bench run`;
    * ``timeout`` — seconds to let one task take before calling it a failure;
    * ``extra_args`` — appended to the command line verbatim;
    * ``env`` — extra environment for the child.
    """

    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self.binary = kwargs.get("binary") or os.environ.get("SYSCITY_BIN") or "syscity"
        self.max_steps = int(kwargs.get("max_steps") or 60)
        self.provider = kwargs.get("provider")
        self.model = kwargs.get("model")
        self.base_url = kwargs.get("base_url")
        self.timeout = float(kwargs.get("timeout") or 1800.0)
        self.extra_args = list(kwargs.get("extra_args") or [])
        self.env = kwargs.get("env")
        self._dir = None

    @staticmethod
    def name() -> str:
        return "syscity"

    @property
    def version(self):
        if self._version:
            return self._version
        try:
            completed = subprocess.run(
                [self.binary, "--version"], capture_output=True, text=True, timeout=20
            )
        except (OSError, subprocess.SubprocessError):
            return None
        lines = (completed.stdout or completed.stderr or "").strip().splitlines()
        return lines[0] if lines else None

    # ── Cua Bench's agent interface ─────────────────────────────────────────

    async def perform_task(
        self,
        task_description: str,
        session,
        logging_dir=None,
        tracer=None,
    ) -> AgentResult:
        if logging_dir is not None:
            self._dir = str(logging_dir)

        sandbox = getattr(session, "sandbox", None)
        if sandbox is None:
            raise SyscityRunFailed(
                "this session has no cua_sandbox.Sandbox, so the bridge cannot run "
                "inside it; use --on local"
            )

        url, tunnel = await start_bridge(sandbox)
        try:
            result = await self._run(task_description, url)
        finally:
            await self._close(tunnel)

        snippets = snippets_from_result(result)
        for index, snippet in enumerate(snippets, start=1):
            logger.info("action %d/%d: %s", index, len(snippets), describe(snippet))
        self._write_result(result)
        self._trace(tracer, task_description, result, snippets)

        if result.get("error"):
            # The model never got a usable turn. Not an answer the evaluator
            # should score as wrong.
            raise SyscityRunFailed(
                "syscity reported an agent error for this task: {0}".format(
                    result["error"]
                )
            )

        logger.info("answer: %s", (result.get("answer") or "").strip()[:2000])
        return AgentResult(
            total_input_tokens=0,
            total_output_tokens=0,
            failure_mode=FailureMode.NONE
            if result.get("stop_reason") == "final_answer"
            else FailureMode.MAX_STEPS_EXCEEDED,
        )

    # ── One syscity run ─────────────────────────────────────────────────────

    async def _run(self, instruction, url):
        result_path = os.path.join(self._out_dir(), "result.json")
        if os.path.exists(result_path):
            os.remove(result_path)

        command = [
            self.binary,
            "bench",
            "run",
            "--instruction",
            instruction,
            "--computer-url",
            url,
            "--task-id",
            "cua-bench",
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
        loop = asyncio.get_running_loop()
        try:
            # The child blocks for the whole task; keep the event loop free so
            # the harness can still cancel.
            completed = await loop.run_in_executor(None, lambda: self._spawn(command))
        except FileNotFoundError as error:
            raise SyscityUnavailable(
                "could not start {0!r}: build it with `cargo build --release` or "
                "set SYSCITY_BIN".format(self.binary)
            ) from error
        except subprocess.TimeoutExpired as error:
            raise SyscityRunFailed(
                "syscity did not finish within {0:.0f}s".format(self.timeout)
            ) from error

        if completed.returncode != 0:
            raise SyscityRunFailed(
                "syscity exited {0}: {1}".format(
                    completed.returncode,
                    (completed.stderr or completed.stdout or "").strip()[-2000:],
                )
            )

        try:
            with open(result_path, "r", encoding="utf-8") as handle:
                return json.load(handle)
        except (OSError, ValueError) as error:
            raise SyscityRunFailed(
                "no readable result at {0}: {1}".format(result_path, error)
            ) from error

    def _spawn(self, command):
        return subprocess.run(
            command,
            capture_output=True,
            text=True,
            timeout=self.timeout,
            env=self._subprocess_env(),
        )

    # ── Files, tracing and teardown ─────────────────────────────────────────

    def _write_result(self, result):
        path = os.path.join(self._out_dir(), "result.json")
        try:
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(result, handle, indent=2)
                handle.write("\n")
        except (OSError, TypeError) as error:
            logger.warning("could not keep a copy of the result at %s: %s", path, error)

    def _trace(self, tracer, instruction, result, snippets):
        """Hand the trajectory to the harness when it asks for one.

        The tracer is Cua Bench's `Tracing` object, whose interface is
        `record(event_name, data, images=None)` — not a callable.

        `output` is a list of `computer_call` items rather than a string, because
        that is the shape `cua_bench/results.py::_tool_calls` reads: a string
        output produces an agent step with no tool call, which renders in the
        ATIF trajectory as an empty turn. Each item carries the snippet as its
        `code` argument, so the trajectory says what was actually sent to the
        guest rather than only what the action was called.
        """
        if tracer is None:
            return
        try:
            for index, snippet in enumerate(snippets, start=1):
                tracer.record(
                    "agent_step",
                    {
                        "step": index,
                        "agent": self.name(),
                        "model": self.model,
                        "output": [
                            {
                                "type": "computer_call",
                                "call_id": "call_{0}".format(index),
                                "action": {
                                    "summary": describe(snippet),
                                    "code": snippet,
                                },
                            }
                        ],
                        "code": snippet,
                    },
                )
            tracer.record(
                "agent_result",
                {
                    "agent": self.name(),
                    "instruction": instruction,
                    "answer": result.get("answer"),
                    "stop_reason": result.get("stop_reason"),
                    "steps": result.get("steps"),
                    "actions": len(snippets),
                },
            )
        except Exception as error:  # noqa: BLE001 - tracing must not fail a task
            logger.debug("could not hand the trajectory to the tracer: %s", error)

    def _out_dir(self):
        if self._dir is None:
            self._dir = tempfile.mkdtemp(prefix="syscity-cua-")
        os.makedirs(self._dir, exist_ok=True)
        return self._dir

    def _subprocess_env(self):
        env = dict(os.environ)
        if self.env:
            env.update(self.env)
        # `--max-steps` is the single source of the budget; an inherited value
        # would silently win on some codepaths.
        env.pop("SYSCITY_MAX_TOOL_ITERATIONS", None)
        return env

    async def _close(self, tunnel):
        if tunnel is None:
            return
        close = getattr(tunnel, "close", None)
        try:
            if close is not None:
                await close()
        except Exception as error:  # noqa: BLE001 - teardown must not raise
            logger.debug("could not close the bridge tunnel: %s", error)


register_agent("syscity")(SyscityAgent)


if __name__ == "__main__":
    # A direct run is a smoke test of the module, not of a task: it proves the
    # import path, the registration and the description heuristic.
    print("syscity agent:", SyscityAgent.name())
    print(
        "snippet summary:",
        describe("import pyautogui\npyautogui.click(1, 2)\n"),
    )
    sys.exit(0)
