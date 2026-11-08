#!/usr/bin/env python3
"""Tests for the Cua Bench agent, with a scripted stand-in for syscity.

Run with no sandbox, no Cua Bench install and no Rust build:

    python3 bench/cua/test_sycity_agent.py

The point is the join, not the model. The questions here are:

* does the command line name the tunneled bridge, the task and the budget;
* does a syscity-side error become an infrastructure failure, never a wrong
  answer (a broken harness and a bad model are different facts, and only one of
  them belongs in a success rate);
* does the bridge get every file it needs, and does a bridge that never comes up
  say why instead of hanging;
* is the trajectory handed to the tracer through the interface it actually has.

`cua_bench` is imported for real when it is installed — the interface being
tested is Cua Bench's — and stubbed when it is not, so a plain `python3` can
run this file.
"""

import asyncio
import json
import os
import stat
import sys
import tempfile
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)


# ── Cua Bench's agent interface, real when present ─────────────────────────


def _install_stub_cua_bench():
    """Bind a minimal `cua_bench.agents` so this file runs without the package.

    Mirrors `cua_bench/agents/base.py`: `BaseAgent.__init__(**kwargs)` setting
    `_version`, an abstract `name()`, the `version` property, the
    `FailureMode` members and the `AgentResult` dataclass.
    """
    from enum import Enum
    from dataclasses import dataclass
    from abc import ABC, abstractmethod

    class FailureMode(Enum):
        UNSET = "unset"
        NONE = "none"
        UNKNOWN = "unknown"
        MAX_STEPS_EXCEEDED = "max_steps_exceeded"
        API_ERROR = "api_error"

    @dataclass
    class AgentResult:
        total_input_tokens: int = 0
        total_output_tokens: int = 0
        failure_mode: FailureMode = FailureMode.UNSET

    class BaseAgent(ABC):
        def __init__(self, **kwargs):
            super().__init__()
            self._version = kwargs.get("version", None)

        @staticmethod
        @abstractmethod
        def name():
            raise NotImplementedError

        @property
        def version(self):
            return self._version

        @abstractmethod
        async def perform_task(self, task_description, session, logging_dir=None, tracer=None):
            raise NotImplementedError

    registry = {}

    def register_agent(name):
        def decorate(cls):
            registry[name] = cls
            return cls

        return decorate

    package = types.ModuleType("cua_bench")
    package.__path__ = []
    agents = types.ModuleType("cua_bench.agents")
    agents.BaseAgent = BaseAgent
    agents.FailureMode = FailureMode
    agents.register_agent = register_agent
    base = types.ModuleType("cua_bench.agents.base")
    base.AgentResult = AgentResult
    agents.base = base
    sys.modules["cua_bench"] = package
    sys.modules["cua_bench.agents"] = agents
    sys.modules["cua_bench.agents.base"] = base


try:  # prefer the real interface; the stub is only a fallback
    import cua_bench.agents  # noqa: F401
except ImportError:
    _install_stub_cua_bench()

import sycity_agent as glue  # noqa: E402
from sycity_agent import (  # noqa: E402
    SyscityAgent,
    SyscityRunFailed,
    SyscityUnavailable,
    describe,
    snippets_from_result,
)

# `pyautogui_shim` lives beside the agent; importing it costs nothing (it only
# shells out when a function is called) and lets the clipboard contract be
# tested on any machine, since `subprocess.Popen` is what gets replaced.
import pyautogui_shim as shim  # noqa: E402
import score  # noqa: E402
import atspi_xml  # noqa: E402

CHECK_YES = "import pyautogui\npyautogui.click(1, 2, button='left')\n"

FAKE_SYSCITY = """#!/usr/bin/env python3
import json, sys

out = sys.argv[sys.argv.index("--out") + 1]
payload = json.loads({payload!r})
payload["argv"] = sys.argv[1:]
payload.setdefault("task_id", "cua-bench")
with open(out, "w", encoding="utf-8") as handle:
    json.dump(payload, handle)
sys.exit({exit_code})
"""


def write_fake_syscity(directory, payload, exit_code=0, name="fake-syscity"):
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(FAKE_SYSCITY.format(payload=json.dumps(payload), exit_code=exit_code))
    os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


# ── Stand-ins for the sandbox ──────────────────────────────────────────────


class CommandResult:
    def __init__(self, returncode=0, stdout="", stderr=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


class TunnelInfo:
    def __init__(self, host="127.0.0.1", port=41234):
        self.host = host
        self.port = port
        self.sandbox_port = 8765
        self.closed = False

    @property
    def url(self):
        return "http://{0}:{1}".format(self.host, self.port)

    async def close(self):
        self.closed = True


class TunnelContext:
    """What `sandbox.tunnel.forward(port)` returns: awaitable, not a coroutine.

    `start_bridge` awaits `__aenter__()` directly, so the fake has to expose it.
    """

    def __init__(self, info):
        self._info = info

    async def __aenter__(self):
        return self._info


class FakeSandbox:
    def __init__(self, upload_error=None, start_result=None, log=""):
        self.uploads = []
        self.commands = []
        self.files = self
        self.shell = self
        self.tunnel = self
        self._upload_error = upload_error
        self._start_result = start_result or CommandResult()
        self._log = log

    async def upload(self, local_path, remote_path):
        if self._upload_error is not None:
            raise self._upload_error
        self.uploads.append((os.path.basename(local_path), remote_path))

    async def run(self, command, **kwargs):
        self.commands.append(command)
        if "cat " in command:
            return CommandResult(stdout=self._log)
        return self._start_result

    def forward(self, port):
        self.forwarded = port
        return TunnelContext(TunnelInfo())


class FakeTracer:
    """`cua_bench.tracing.Tracing` in the one respect that matters: `record`."""

    def __init__(self):
        self.events = []

    def record(self, event_name, data_dict, data_images=None):
        self.events.append((event_name, data_dict))


class FakeSession:
    def __init__(self, sandbox):
        self.sandbox = sandbox


def run(coroutine):
    return asyncio.run(coroutine)


# ── Reading syscity's result ───────────────────────────────────────────────


class DescribeTest(unittest.TestCase):
    def test_names_the_operation_not_the_import(self):
        self.assertEqual(describe(CHECK_YES), "pyautogui.click(1, 2, button='left')")

    def test_a_bare_import_is_still_something(self):
        self.assertEqual(describe("import pyautogui\n"), "import pyautogui")

    def test_empty_is_empty(self):
        self.assertEqual(describe("   \n\n "), "")


class SnippetsFromResultTest(unittest.TestCase):
    def test_absent_actions_are_no_actions(self):
        self.assertEqual(snippets_from_result({}), [])
        self.assertEqual(snippets_from_result({"actions": None}), [])
        self.assertEqual(snippets_from_result({"actions": []}), [])

    def test_order_is_preserved_and_blanks_dropped(self):
        self.assertEqual(
            snippets_from_result({"actions": ["a\n", "  ", "b\n"]}),
            ["a\n", "b\n"],
        )

    def test_the_wrong_shape_is_a_harness_failure(self):
        with self.assertRaises(SyscityRunFailed):
            snippets_from_result({"actions": "pyautogui.click(1, 2)"})
        with self.assertRaises(SyscityRunFailed):
            snippets_from_result({"actions": [1, 2, 3]})


# ── One syscity run ────────────────────────────────────────────────────────


class RunTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = self._tmp.name
        self.addCleanup(self._tmp.cleanup)

    def agent(self, binary, **kwargs):
        return SyscityAgent(binary=binary, max_steps=7, timeout=30, **kwargs)

    def test_the_bridge_task_and_budget_reach_the_command_line(self):
        binary = write_fake_syscity(
            self.dir, {"stop_reason": "final_answer", "answer": "", "actions": []}
        )
        agent = self.agent(binary, provider="deepseek", model="deepseek-v4-flash")
        agent._dir = self.dir
        run(agent._run("do it", "http://127.0.0.1:41234"))

        with open(os.path.join(self.dir, "result.json"), "r", encoding="utf-8") as handle:
            argv = json.load(handle)["argv"]

        self.assertEqual(argv[0], "bench")
        self.assertEqual(argv[1], "run")
        self.assertEqual(argv[argv.index("--computer-url") + 1], "http://127.0.0.1:41234")
        self.assertEqual(argv[argv.index("--instruction") + 1], "do it")
        self.assertEqual(argv[argv.index("--task-id") + 1], "cua-bench")
        self.assertEqual(argv[argv.index("--max-steps") + 1], "7")
        self.assertEqual(argv[argv.index("--provider") + 1], "deepseek")
        self.assertEqual(argv[argv.index("--model") + 1], "deepseek-v4-flash")

    def test_a_missing_provider_override_is_not_passed_through(self):
        binary = write_fake_syscity(
            self.dir, {"stop_reason": "final_answer", "answer": "", "actions": []}
        )
        agent = self.agent(binary)
        agent._dir = self.dir
        run(agent._run("do it", "http://127.0.0.1:41234"))

        with open(os.path.join(self.dir, "result.json"), "r", encoding="utf-8") as handle:
            argv = json.load(handle)["argv"]
        self.assertNotIn("--provider", argv)
        self.assertNotIn("--model", argv)

    def test_a_failed_process_is_an_infrastructure_error(self):
        binary = write_fake_syscity(
            self.dir, {"stop_reason": "final_answer", "answer": ""}, exit_code=2
        )
        agent = self.agent(binary)
        agent._dir = self.dir
        with self.assertRaises(SyscityRunFailed):
            run(agent._run("do it", "http://127.0.0.1:41234"))

    def test_a_binary_that_does_not_exist_is_named_as_such(self):
        agent = self.agent(os.path.join(self.dir, "not-built"))
        agent._dir = self.dir
        with self.assertRaises(SyscityUnavailable):
            run(agent._run("do it", "http://127.0.0.1:41234"))

    def test_an_unreadable_result_is_an_infrastructure_error(self):
        binary = os.path.join(self.dir, "half-written")
        with open(binary, "w", encoding="utf-8") as handle:
            handle.write("#!/usr/bin/env python3\nimport sys\nsys.exit(0)\n")
        os.chmod(binary, 0o755)
        agent = self.agent(binary)
        agent._dir = self.dir
        with self.assertRaises(SyscityRunFailed):
            run(agent._run("do it", "http://127.0.0.1:41234"))

    def test_the_inherited_budget_is_removed_from_the_child_environment(self):
        agent = self.agent("syscity")
        agent.env = {"SYSCITY_MAX_TOOL_ITERATIONS": "999", "KEEP_ME": "1"}
        env = agent._subprocess_env()
        self.assertNotIn("SYSCITY_MAX_TOOL_ITERATIONS", env)
        self.assertEqual(env["KEEP_ME"], "1")


# ── The join ───────────────────────────────────────────────────────────────


class PerformTaskTest(unittest.TestCase):
    """`perform_task` end to end, with `start_bridge` replaced by a fixed URL.

    The bridge itself is covered below; what is under test here is what the
    agent reports once syscity has finished.
    """

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = self._tmp.name
        self.addCleanup(self._tmp.cleanup)
        self._saved = glue.start_bridge

    def tearDown(self):
        glue.start_bridge = self._saved

    def point_at(self, url="http://127.0.0.1:41234"):
        async def fake(sandbox):
            return url, None

        glue.start_bridge = fake

    def agent(self, binary):
        return SyscityAgent(binary=binary, max_steps=7, timeout=30)

    def test_a_final_answer_is_not_a_failure(self):
        binary = write_fake_syscity(
            self.dir,
            {
                "stop_reason": "final_answer",
                "answer": "clicked",
                "actions": [CHECK_YES],
            },
        )
        self.point_at()
        agent = self.agent(binary)
        result = run(
            agent.perform_task("do it", FakeSession(FakeSandbox()), logging_dir=self.dir)
        )
        self.assertEqual(result.failure_mode, glue.FailureMode.NONE)
        with open(os.path.join(self.dir, "result.json"), "r", encoding="utf-8") as handle:
            self.assertEqual(json.load(handle)["answer"], "clicked")

    def test_an_exhausted_budget_is_reported_as_such(self):
        binary = write_fake_syscity(
            self.dir, {"stop_reason": "max_steps", "answer": "", "actions": []}
        )
        self.point_at()
        agent = self.agent(binary)
        result = run(
            agent.perform_task("do it", FakeSession(FakeSandbox()), logging_dir=self.dir)
        )
        self.assertEqual(result.failure_mode, glue.FailureMode.MAX_STEPS_EXCEEDED)

    def test_an_agent_side_error_is_not_a_wrong_answer(self):
        binary = write_fake_syscity(
            self.dir,
            {"stop_reason": "error", "answer": "", "actions": [], "error": "provider 502"},
        )
        self.point_at()
        agent = self.agent(binary)
        with self.assertRaises(SyscityRunFailed):
            run(agent.perform_task("do it", FakeSession(FakeSandbox()), logging_dir=self.dir))

    def test_a_session_without_a_sandbox_is_named_rather_than_crashed(self):
        class NoSandbox:
            sandbox = None

        agent = self.agent("syscity")
        with self.assertRaises(SyscityRunFailed) as caught:
            run(agent.perform_task("do it", NoSandbox(), logging_dir=self.dir))
        self.assertIn("--on local", str(caught.exception))

    def test_the_trajectory_reaches_the_tracer_through_record(self):
        binary = write_fake_syscity(
            self.dir,
            {
                "stop_reason": "final_answer",
                "answer": "clicked",
                "actions": [CHECK_YES],
            },
        )
        self.point_at()
        agent = self.agent(binary)
        tracer = FakeTracer()
        run(
            agent.perform_task(
                "do it", FakeSession(FakeSandbox()), logging_dir=self.dir, tracer=tracer
            )
        )

        names = [name for name, _ in tracer.events]
        self.assertIn("agent_step", names)
        self.assertIn("agent_result", names)
        step = dict(tracer.events)["agent_step"]
        # `output` must be a list of `computer_call` items: that is what
        # cua_bench/results.py::_tool_calls turns into a trajectory tool call.
        call = step["output"][0]
        self.assertEqual(call["type"], "computer_call")
        self.assertEqual(call["action"]["summary"], "pyautogui.click(1, 2, button='left')")
        self.assertIn("pyautogui.click", call["action"]["code"])

    def test_a_tracer_that_raises_does_not_fail_the_task(self):
        class Broken:
            def record(self, *args, **kwargs):
                raise RuntimeError("tracing is down")

        binary = write_fake_syscity(
            self.dir, {"stop_reason": "final_answer", "answer": "", "actions": [CHECK_YES]}
        )
        self.point_at()
        agent = self.agent(binary)
        result = run(
            agent.perform_task(
                "do it", FakeSession(FakeSandbox()), logging_dir=self.dir, tracer=Broken()
            )
        )
        self.assertEqual(result.failure_mode, glue.FailureMode.NONE)


# ── The guest bridge ───────────────────────────────────────────────────────


class StartBridgeTest(unittest.TestCase):
    def setUp(self):
        self._saved = glue.wait_for_bridge

        async def answered(sandbox, url, timeout=30.0):
            return None

        glue.wait_for_bridge = answered

    def tearDown(self):
        glue.wait_for_bridge = self._saved

    def test_every_bridge_file_is_uploaded_and_the_tunnel_is_returned(self):
        sandbox = FakeSandbox()
        url, tunnel = run(glue.start_bridge(sandbox))

        uploaded = [name for name, _ in sandbox.uploads]
        self.assertEqual(sorted(uploaded), sorted(glue.BRIDGE_FILES))
        for name, remote in sandbox.uploads:
            self.assertEqual(remote, "{0}/{1}".format(glue.GUEST_DIR, name))
        self.assertEqual(sandbox.forwarded, glue.BRIDGE_PORT)
        self.assertEqual(url, "http://127.0.0.1:41234")
        self.assertIsNotNone(tunnel)

    def test_the_bridge_is_started_detached(self):
        sandbox = FakeSandbox()
        run(glue.start_bridge(sandbox))
        self.assertTrue(any("nohup" in command for command in sandbox.commands))

    def test_a_bridge_that_cannot_start_says_why(self):
        sandbox = FakeSandbox(
            start_result=CommandResult(returncode=127, stderr="python3: not found")
        )
        with self.assertRaises(SyscityRunFailed) as caught:
            run(glue.start_bridge(sandbox))
        self.assertIn("python3: not found", str(caught.exception))

    def test_a_missing_bridge_file_is_named(self):
        real = glue.BRIDGE_FILES
        glue.BRIDGE_FILES = real + ("not_a_real_file.py",)
        self.addCleanup(setattr, glue, "BRIDGE_FILES", real)
        with self.assertRaises(SyscityRunFailed) as caught:
            run(glue.start_bridge(FakeSandbox()))
        self.assertIn("not_a_real_file.py", str(caught.exception))


class WaitForBridgeTest(unittest.TestCase):
    def test_a_bridge_that_never_answers_reports_its_log(self):
        # Port 1 on loopback refuses immediately, so this costs a fraction of
        # the timeout rather than the whole of it.
        sandbox = FakeSandbox(log="bridge listening on 0.0.0.0:8765")
        with self.assertRaises(SyscityRunFailed) as caught:
            run(glue.wait_for_bridge(sandbox, "http://127.0.0.1:1", timeout=0.5))
        message = str(caught.exception)
        self.assertIn("did not answer", message)
        self.assertIn("bridge listening", message)


class KeyNameTest(unittest.TestCase):
    """Every key name must become an X keysym, or be refused.

    `xdotool` warns `No such key name` on stderr and still exits 0, so a name it
    does not know becomes a keypress that never happened while every layer above
    believes it did. A date-picker run sent `page_up`, got success, and moved
    nothing.
    """

    #: The names that appear in real syscity snippets, with the keysym X wants.
    EXPECTED = {
        "enter": "Return",
        "return": "Return",
        "esc": "Escape",
        "escape": "Escape",
        "delete": "Delete",
        "backspace": "BackSpace",
        "tab": "Tab",
        "space": "space",
        "insert": "Insert",
        "home": "Home",
        "end": "End",
        "up": "Up",
        "down": "Down",
        "left": "Left",
        "right": "Right",
        "pageup": "Prior",
        "page_up": "Prior",
        "pagedown": "Next",
        "page_down": "Next",
        "prior": "Prior",
        "next": "Next",
        "f1": "F1",
        "f12": "F12",
        "printscreen": "Print",
        "prtsc": "Print",
        "prntscrn": "Print",
        "ctrl": "ctrl",
        "cmd": "ctrl",
        "win": "super",
        "option": "alt",
        "capslock": "Caps_Lock",
        "media": None,
    }

    def test_real_snippets_map_to_real_keysyms(self):
        for name, expected in self.EXPECTED.items():
            if expected is None:
                continue
            self.assertEqual(shim._key_name(name), expected, name)

    def test_a_single_character_is_itself(self):
        for character in ["a", "Z", "7", "/", " ", "é"]:
            self.assertEqual(shim._key_name(character), character)

    def test_the_page_keys_are_not_forwarded_verbatim(self):
        # These are the exact strings xdotool rejects; forwarding them was the
        # bug, so they must not come back out of `_key_name` unchanged.
        for name in ["page_up", "page_down", "pageup", "pagedown"]:
            self.assertNotIn(shim._key_name(name), ("page_up", "page_down", "pageup", "pagedown"))

    def test_an_unknown_name_raises(self):
        for name in ["bogus_key", "again", "KP_Add", "not a key"]:
            with self.assertRaises(RuntimeError):
                shim._key_name(name)

    def test_every_mapping_is_itself_accepted(self):
        # A mapping to a keysym xdotool also rejects would be the same silence
        # one level down. `_key_name` of every target must be a no-op.
        for target in sorted(set(shim._KEY_ALIASES.values())):
            self.assertEqual(shim._key_name(target), target, target)

    def test_the_modifier_map_covers_every_pyautogui_spelling(self):
        for name in ["ctrl", "control", "cmd", "command", "meta"]:
            self.assertEqual(shim._key_name(name), "ctrl", name)
        for name in ["win", "super", "winleft", "winright"]:
            self.assertEqual(shim._key_name(name), "super", name)


class SilentDropTest(unittest.TestCase):
    """`_xdotool` must not treat a rejected key name as a success."""

    def setUp(self):
        self._saved = shim._run
        self.addCleanup(setattr, shim, "_run", self._saved)

    def _answer(self, stdout, stderr, code):
        import subprocess as real

        shim._run = lambda argv, input_text=None: real.CompletedProcess(
            argv, code, stdout, stderr
        )

    def test_an_ignored_key_name_raises_even_with_exit_zero(self):
        self._answer(b"", b"(symbol) No such key name 'page_up'. Ignoring it.", 0)
        with self.assertRaises(RuntimeError) as caught:
            shim._xdotool("key", "page_up")
        self.assertIn("No such key name", str(caught.exception))

    def test_a_clean_call_returns_stdout(self):
        self._answer(b"1280 800\n", b"", 0)
        self.assertEqual(shim._xdotool("getdisplaygeometry"), "1280 800\n")

    def test_a_non_zero_exit_still_raises(self):
        self._answer(b"", b"cannot open display", 1)
        with self.assertRaises(RuntimeError):
            shim._xdotool("getdisplaygeometry")


class WheelTest(unittest.TestCase):
    """A scroll must not spend xdotool's default delay per wheel click.

    syscity turns one scroll `amount` into 100 wheel clicks. `xdotool click`
    waits 100 ms between repeats unless told otherwise, so the default made a
    single scroll take tens of seconds, past the remote client's 30 s timeout —
    which the model then read as "the endpoint refuses scroll" (fill-form v2).
    The distance is identical either way; only the wall clock changes.
    """

    def setUp(self):
        self._saved = shim._xdotool
        self.calls = []
        shim._xdotool = lambda *args: self.calls.append(args) or ""
        self.addCleanup(setattr, shim, "_xdotool", self._saved)

    def test_a_scroll_passes_no_inter_click_delay(self):
        shim.scroll(-500, x=600, y=400)
        click = [call for call in self.calls if call[:1] == ("click",)][-1]
        self.assertIn("--delay", click)
        self.assertEqual(click[click.index("--delay") + 1], "0")
        self.assertEqual(click[-2:], (500, 5))

    def test_wheel_up_is_button_four(self):
        shim.scroll(3)
        self.assertEqual(self.calls[-1][-1], 4)

    def test_horizontal_scroll_uses_the_same_delay(self):
        shim.hscroll(-2)
        self.assertEqual(self.calls[-1][-1], 7)
        self.assertEqual(self.calls[-1][self.calls[-1].index("--delay") + 1], "0")

    def test_a_zero_scroll_still_sends_one_click(self):
        shim.scroll(0)
        self.assertEqual(self.calls[-1][-2:], (1, 5))


class DragTest(unittest.TestCase):
    """A drag must produce a *sequence*, not two endpoints.

    Chromium begins a native drag once a press has been followed by enough
    motion events and abandons it — never firing `drop` — if the pointer merely
    teleports from origin to target. A `drag_to` that moves in one jump looks
    exactly like a click from the page's point of view.
    """

    def setUp(self):
        self._saved = (shim._xdotool, shim.time.sleep)
        self.moved = []

        def record(*args):
            if args[:1] == ("getmouselocation",):
                return "X=200\nY=300\n"  # the origin a drag interpolates from
            self.moved.append(args)
            return ""

        shim._xdotool = record
        shim.time.sleep = lambda seconds: None
        self.addCleanup(self._restore)

    def _restore(self):
        shim._xdotool, shim.time.sleep = self._saved

    def test_a_drag_moves_in_many_points(self):
        shim.moveTo(200, 300, duration=0.5)
        self.assertGreaterEqual(len(self.moved), shim.DRAG_MIN_STEPS)
        self.assertEqual(
            [command for command, _, _ in self.moved[:1]], ["mousemove"]
        )
        self.assertEqual(self.moved[-1], ("mousemove", 200, 300))

    def test_the_points_are_evenly_spread_and_monotonic(self):
        # The stub reports the pointer at (200, 300), so a move to (100, 200)
        # walks x downwards — every step strictly between origin and target.
        shim.moveTo(100, 200, duration=0.4)
        xs = [arguments[0] for command, *arguments in self.moved]
        self.assertEqual(xs, sorted(xs, reverse=True))
        self.assertLess(xs[0], 200)  # starts near the origin, not at the target
        self.assertEqual(xs[-1], 100)

    def test_a_plain_move_is_one_event(self):
        shim.moveTo(50, 60)
        self.assertEqual(self.moved, [("mousemove", 50, 60)])


class ClipboardTest(unittest.TestCase):
    """`copy` must either own the selection or raise.

    The failure this pins down was silent: `copy` returned in milliseconds,
    owning nothing, so every `try: copy(); paste() except: write()` snippet took
    the paste branch and dropped the text. Three separate mistakes caused it and
    any one of them alone is enough to lose a paste.
    """

    def setUp(self):
        self._saved = (
            shim.subprocess.Popen,
            shim._run,
            shim.CLIPBOARD_SETTLE,
            shim.shutil.which,
        )
        self.started = []

        class FakeStdin:
            def __init__(self):
                self.written = b""

            def write(self, data):
                self.written += data

            def close(self):
                pass

        class FakeProcess:
            def __init__(self):
                self.stdin = FakeStdin()

        def fake_popen(argv, **kwargs):
            process = FakeProcess()
            self.started.append((argv, kwargs, process))
            return process

        shim.subprocess.Popen = fake_popen
        shim.CLIPBOARD_SETTLE = 0.2
        shim.shutil.which = lambda name: "/usr/bin/xclip"
        self.addCleanup(self._restore)

    def _restore(self):
        (
            shim.subprocess.Popen,
            shim._run,
            shim.CLIPBOARD_SETTLE,
            shim.shutil.which,
        ) = self._saved

    def serve(self, payload):
        import subprocess as real

        shim._run = lambda argv, input_text=None: real.CompletedProcess(
            argv, 0, payload, b""
        )

    def test_the_text_goes_in_on_stdin_not_a_path(self):
        self.started.clear()
        self.serve(b"typed")
        shim.copy("typed")
        argv, kwargs, process = self.started[0]
        # A path argument would be opened by the child after `Popen` returns,
        # which is exactly how the temp file got unlinked out from under it.
        self.assertNotIn("typed", " ".join(argv))
        self.assertNotIn("cua-clip-", " ".join(argv))
        self.assertEqual(kwargs.get("stdin"), shim.subprocess.PIPE)
        self.assertEqual(process.stdin.written, b"typed")

    def test_the_owner_serves_more_than_one_request(self):
        self.started.clear()
        self.serve(b"typed")
        shim.copy("typed")
        argv = self.started[0][0]
        # `-loops 1` answers the TARGETS probe a browser makes before the paste
        # and then exits, so the paste itself reads nothing.
        self.assertEqual(argv[argv.index("-loops") + 1], "0")

    def test_a_copy_that_owns_nothing_raises(self):
        # The read-back never matches: the snippet must be able to learn that
        # the clipboard is empty and fall back to typing.
        self.serve(b"")
        with self.assertRaises(RuntimeError) as caught:
            shim.copy("typed")
        self.assertIn("not serving the copied text", str(caught.exception))

    def test_a_copy_that_lands_returns_quietly(self):
        self.serve(b"typed")
        shim.copy("typed")  # no raise

    def test_an_empty_copy_needs_no_read_back(self):
        self.started.clear()
        shim.copy("")  # no raise, and no claim either way
        self.assertEqual(len(self.started), 1)


class ScoreTest(unittest.TestCase):
    """The environment ceiling must never excuse the model.

    A task leaves the denominator only on evidence — its official oracle failing
    every variant, and the model never solving any — and never merely because
    the model failed it. These tests are the guard rails on that rule, and each
    one corresponds to a way the number could otherwise be inflated.
    """

    def ceiling(self, oracle):
        return score.Ceiling(oracle={task: dict(v) for task, v in oracle.items()})

    def summary(self, rows, avg_reward=None):
        return {"total": len(rows), "avg_reward": avg_reward, "results": rows}

    def row(self, task, variant, reward, error=None):
        return {"task": task, "variant": variant, "reward": reward, "error": error}

    def test_excludes_a_task_the_oracle_and_model_both_fail(self):
        ceiling = self.ceiling({"drag-drop": {0: 0.0, 1: 0.0}})
        rows = [self.row("drag-drop", 0, 0.0), self.row("drag-drop", 1, 0.0),
                self.row("click-button", 0, 1.0)]
        rep = score.report(self.summary(rows), ceiling)
        self.assertEqual(rep.excluded, ["drag-drop"])
        self.assertEqual((rep.reachable_passed, rep.reachable_total), (1, 1))

    def test_a_model_success_vetoes_the_exclusion(self):
        # The real date-picker case: the oracle scored 0 on every variant, but
        # the model solved v4, so the task is reachable and the declaration is
        # wrong. It must not be subtracted.
        ceiling = self.ceiling({"date-picker": {0: 0.0, 4: 0.0}})
        rows = [self.row("date-picker", 0, 0.0), self.row("date-picker", 4, 1.0)]
        rep = score.report(self.summary(rows), ceiling)
        self.assertEqual(rep.excluded, [])
        self.assertEqual(rep.reachable_total, 2)
        self.assertTrue(any("model solved v4" in w for w in rep.warnings))

    def test_an_unmeasured_oracle_is_not_a_ceiling(self):
        ceiling = self.ceiling({"drag-drop": {0: None}})
        rep = score.report(self.summary([self.row("drag-drop", 0, 0.0)]), ceiling)
        self.assertEqual(rep.excluded, [])
        self.assertEqual(rep.reachable_total, 1)
        self.assertTrue(any("unmeasured" in w for w in rep.warnings))

    def test_a_partly_reachable_task_is_not_excluded(self):
        # Excluding per task is all-or-nothing: one oracle pass is enough to
        # show the task is reachable here.
        ceiling = self.ceiling({"fill-form": {0: 1.0, 1: 0.0}})
        rows = [self.row("fill-form", 0, 0.0), self.row("fill-form", 1, 0.0)]
        rep = score.report(self.summary(rows), ceiling)
        self.assertEqual(rep.excluded, [])
        self.assertEqual(rep.reachable_total, 2)

    def test_a_declared_task_absent_from_the_run_is_ignored(self):
        ceiling = self.ceiling({"drag-drop": {0: 0.0}})
        rep = score.report(self.summary([self.row("click-button", 0, 1.0)]), ceiling)
        self.assertEqual(rep.excluded, [])
        self.assertEqual(rep.warnings, [])

    def test_raw_is_the_summary_number(self):
        rows = [self.row("a", 0, 1.0), self.row("a", 1, 0.0)]
        rep = score.report(self.summary(rows, avg_reward=0.75), score.Ceiling())
        self.assertEqual(rep.raw, 0.75)  # the run's own figure, not recomputed
        self.assertEqual(rep.reachable, 0.5)

    def test_an_errored_variant_is_not_scored(self):
        rows = [self.row("a", 0, 1.0), self.row("a", 1, None, error="boom")]
        rep = score.report(self.summary(rows), score.Ceiling())
        self.assertEqual(rep.scored, 1)
        self.assertEqual(rep.raw, 1.0)
        self.assertEqual(rep.errored, ["a v1: boom"])

    def test_variant_keys_may_be_strings(self):
        # JSON object keys are strings; loading must still match an int variant.
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "ceiling.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(
                    {"unreachable_tasks": {"drag-drop": {"oracle_reward": {"3": 0.0}}}},
                    handle,
                )
            ceiling = score.Ceiling.load(path)
        self.assertEqual(ceiling.oracle["drag-drop"], {3: 0.0})

    def test_the_shipped_ceiling_loads_and_lists_drag_drop(self):
        ceiling = score.Ceiling.load(score.__file__.rsplit("/", 1)[0] + "/ceiling.json")
        self.assertIn("drag-drop", ceiling.oracle)
        # date-picker cost me a wrong answer once; it must stay out.
        self.assertNotIn("date-picker", ceiling.oracle)


class OptionBoxTest(unittest.TestCase):
    """A closed `<select>` reports every option at the combo box's own box.

    Clicking "Banana" there just closes the menu, which is how `select-dropdown
    v1` was lost. When the popup opens, the real rows live in a top-level
    `window` → `tree table`; `real_option_boxes` has to find them without
    disturbing a tree that was already correct.
    """

    COMBO = (213, 187, 185, 52)

    def nodes(self, popup_open):
        """The shape a real pywebview `<select>` reports, open or closed."""
        nodes = [
            ("application", "gw3.py", None),
            ("frame", "S", (200, 150, 510, 334)),
            ("combo box", "Fruit selection dropdown", self.COMBO),
            ("menu", None, None),
            ("menu item", "-- Select a fruit --", self.COMBO),
            ("menu item", "Apple", self.COMBO),
            ("menu item", "Banana", self.COMBO),
        ]
        if popup_open:
            nodes += [
                ("window", None, (213, 239, 185, 102)),
                ("tree table", None, (214, 240, 183, 100)),
                ("table cell", "-- Select a fruit --", (236, 241, 159, 23)),
                ("table cell", "Apple", (236, 266, 159, 23)),
                ("table cell", "Banana", (236, 291, 159, 23)),
            ]
        return nodes

    def test_an_open_popup_gives_each_option_its_real_box(self):
        boxes = atspi_xml.real_option_boxes(self.nodes(popup_open=True))
        self.assertEqual(boxes["Banana"], (236, 291, 159, 23))
        self.assertEqual(boxes["Apple"], (236, 266, 159, 23))
        self.assertNotIn("Fruit selection dropdown", boxes)

    def test_a_closed_popup_changes_nothing(self):
        # The popup is a 0x0 menu, so there is no real box to substitute and the
        # tree must come out exactly as the platform reported it.
        self.assertEqual(atspi_xml.real_option_boxes(self.nodes(popup_open=False)), {})

    def test_a_distinct_reported_box_is_believed(self):
        # A page that really draws its rows in place reports distinct boxes; the
        # popup must not overwrite them.
        nodes = self.nodes(popup_open=True)
        nodes = [
            (r, n, (900, 900, 10, 10) if (r, n) == ("menu item", "Apple") else b)
            for r, n, b in nodes
        ]
        boxes = atspi_xml.real_option_boxes(nodes)
        self.assertNotIn("Apple", boxes)
        self.assertIn("Banana", boxes)

    def test_an_ambiguous_label_is_skipped(self):
        # Two rows in the popup with the same label: no way to know which one
        # the caller means, so neither is offered.
        nodes = self.nodes(popup_open=True)
        nodes.append(("table cell", "Banana", (236, 316, 159, 23)))
        self.assertNotIn("Banana", atspi_xml.real_option_boxes(nodes))

    def test_the_popup_rows_are_not_mistaken_for_the_combo_boxes(self):
        # The popup follows the combo box in tree order; treating its rows as
        # more options of the combo box made the substitution never fire. A
        # label unique to the popup proves the rows were read as the popup's.
        nodes = self.nodes(popup_open=True)
        nodes += [
            ("combo box", "Second", (300, 400, 100, 30)),
            ("menu item", "Pear", (300, 400, 100, 30)),
            ("window", None, (300, 430, 100, 60)),
            ("table cell", "Pear", (310, 440, 80, 20)),
        ]
        boxes = atspi_xml.real_option_boxes(nodes)
        self.assertEqual(boxes["Pear"], (310, 440, 80, 20))


if __name__ == "__main__":
    unittest.main(verbosity=2)
