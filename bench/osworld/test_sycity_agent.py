#!/usr/bin/env python3
"""Tests for both agents, with scripted stand-ins for the syscity binary.

Run with no OSWorld checkout and no Rust build:

    python3 bench/osworld/test_sycity_agent.py

The point is the join, not the model: that the command line names the VM, that
the snippets come back in order, that a syscity-side error is raised as an
infrastructure failure instead of a `FAIL` action, and that infrastructure
errors stay out of the reported denominator. For the step agent the extra
question is ordering — the `result` line for an action must go out before the
next one is read, or syscity's own loop is being run against a stale screen.
"""

import json
import os
import stat
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from sycity_agent import (  # noqa: E402
    SyscityAgent,
    SyscityRunFailed,
    SyscityStepAgent,
    summarize,
)

FAKE_SYSCITY = """#!/usr/bin/env python3
import json, sys

out = sys.argv[sys.argv.index("--out") + 1]
payload = json.loads({payload!r})
payload["argv"] = sys.argv[1:]
with open(out, "w", encoding="utf-8") as handle:
    json.dump(payload, handle)
sys.exit({exit_code})
"""

# Speaks the `bench serve` protocol. `FAKE_SERVE_SCRIPT` is the list of turns
# the harness expects to be asked for, one per line received; `FAKE_SERVE_LOG`
# is where every line received (and how it ended) is recorded for assertions.
FAKE_SERVE = """#!/usr/bin/env python3
import json, os, sys

with open(os.environ["FAKE_SERVE_SCRIPT"], "r", encoding="utf-8") as handle:
    SCRIPT = json.load(handle)
LOG = os.environ["FAKE_SERVE_LOG"]


def log(kind, detail):
    with open(LOG, "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"kind": kind, "detail": detail}) + "\\n")


def send(payload):
    sys.stdout.write(json.dumps(payload) + "\\n")
    sys.stdout.flush()


log("argv", sys.argv[1:])
log("budget", os.environ.get("SYSCITY_MAX_TOOL_ITERATIONS"))
send({"type": "ready", "schema": 1})

remaining = list(SCRIPT)
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    log("recv", message)
    if not remaining:
        log("unexpected", message)
        sys.exit(3)
    step = remaining.pop(0)
    if message.get("type") != step["expect"]:
        log("out-of-order", {"expected": step["expect"], "got": message.get("type")})
        sys.exit(4)
    for payload in step.get("send", []):
        send(payload)

if remaining:
    log("unfinished", remaining)
    sys.exit(5)
log("exit", None)
"""


def _make_executable(path, source):
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(source)
    os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)
    return path


def write_fake_syscity(directory, payload, exit_code=0):
    return _make_executable(
        os.path.join(directory, "fake-syscity"),
        FAKE_SYSCITY.format(payload=json.dumps(payload), exit_code=exit_code),
    )


def write_fake_serve(directory):
    """A `syscity bench serve` stand-in; its turns come from `FAKE_SERVE_SCRIPT`.

    Each turn is `{"expect": <line type>, "send": [<line>, ...]}`.
    """
    return _make_executable(os.path.join(directory, "fake-serve"), FAKE_SERVE)


class ReplayTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = self._tmp.name
        self.addCleanup(self._tmp.cleanup)

    def agent(self, binary):
        agent = SyscityAgent(binary=binary, max_steps=7, artifacts_dir=self.dir)
        agent.task_id = "abc123"
        agent.reset(vm_ip="10.0.0.5")
        return agent

    def test_snippets_are_replayed_in_order_then_done(self):
        binary = write_fake_syscity(
            self.dir,
            {
                "stop_reason": "final_answer",
                "answer": "done",
                "actions": [
                    "import pyautogui\npyautogui.click(1, 2, button='left')\n",
                    "import pyautogui\npyautogui.write('hi')\n",
                ],
            },
        )
        agent = self.agent(binary)

        _, first = agent.predict("open the file", {})
        _, second = agent.predict("open the file", {})
        _, third = agent.predict("open the file", {})

        self.assertIn("pyautogui.click(1, 2", first[0])
        self.assertIn("pyautogui.write('hi')", second[0])
        self.assertEqual(third, ["DONE"])

    def test_the_vm_address_and_budget_reach_the_command_line(self):
        binary = write_fake_syscity(
            self.dir, {"stop_reason": "final_answer", "answer": "", "actions": []}
        )
        agent = self.agent(binary)
        agent.predict("do it", {})

        with open(os.path.join(self.dir, "result.json"), "r", encoding="utf-8") as handle:
            argv = json.load(handle)["argv"]
        self.assertIn("--computer-url", argv)
        self.assertEqual(argv[argv.index("--computer-url") + 1], "http://10.0.0.5:5000")
        self.assertEqual(argv[argv.index("--max-steps") + 1], "7")
        self.assertEqual(argv[argv.index("--task-id") + 1], "abc123")

    def test_an_exhausted_budget_ends_in_fail_rather_than_done(self):
        binary = write_fake_syscity(
            self.dir, {"stop_reason": "max_steps", "answer": "", "actions": []}
        )
        agent = self.agent(binary)
        _, actions = agent.predict("do it", {})
        self.assertEqual(actions, ["FAIL"])

    def test_a_failed_process_is_an_infrastructure_error(self):
        binary = write_fake_syscity(
            self.dir, {"stop_reason": "final_answer", "answer": "", "actions": []}, exit_code=2
        )
        agent = self.agent(binary)
        with self.assertRaises(SyscityRunFailed):
            agent.predict("do it", {})

    def test_an_agent_side_error_is_not_a_wrong_answer(self):
        binary = write_fake_syscity(
            self.dir,
            {"stop_reason": "error", "answer": "", "actions": [], "error": "provider 502"},
        )
        agent = self.agent(binary)
        with self.assertRaises(SyscityRunFailed):
            agent.predict("do it", {})

    def test_infrastructure_errors_stay_out_of_the_success_rate(self):
        report = summarize(["a", "b"], [1, 0], ["chrome/1: boom"])
        self.assertEqual(report["scored"], 2)
        self.assertEqual(report["passed"], 1)
        self.assertEqual(report["success_rate"], 0.5)
        self.assertEqual(report["infra_errors"], 1)

    def test_no_scores_is_reported_as_unknown_not_as_zero(self):
        report = summarize([], [], ["vm/1: no kvm"])
        self.assertIsNone(report["success_rate"])


CLICK = "import pyautogui\npyautogui.click(1, 2, button='left')\n"
TYPE = "import pyautogui\npyautogui.write('hi')\n"


def finished(answer="done", stop_reason="final_answer", error=None):
    result = {
        "schema": 1,
        "task_id": "abc123",
        "instruction": "do it",
        "answer": answer,
        "stop_reason": stop_reason,
        "steps": 1,
        "tool_calls": 1,
        "actions": [CLICK],
        "elapsed_ms": 5,
        "provider": "deepseek",
        "model": "deepseek-v4-flash",
        "error": error,
    }
    return {"type": "done", "schema": 1, "result": result}


class StepTest(unittest.TestCase):
    """The step protocol: one action per harness turn, in the harness's order."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = self._tmp.name
        self.addCleanup(self._tmp.cleanup)
        self.log_path = os.path.join(self.dir, "serve.log")

    def agent(self, turns, timeout=60.0, env=None):
        binary = write_fake_serve(self.dir)
        script_path = os.path.join(self.dir, "turns.json")
        with open(script_path, "w", encoding="utf-8") as handle:
            json.dump(turns, handle)
        subprocess_env = {
            "FAKE_SERVE_SCRIPT": script_path,
            "FAKE_SERVE_LOG": self.log_path,
        }
        subprocess_env.update(env or {})
        agent = SyscityStepAgent(
            binary=binary,
            max_steps=7,
            timeout=timeout,
            artifacts_dir=self.dir,
            env=subprocess_env,
        )
        agent.task_id = "abc123"
        agent.reset(vm_ip="10.0.0.5")
        self.addCleanup(agent.close)
        return agent

    def lines(self):
        if not os.path.exists(self.log_path):
            return []
        with open(self.log_path, "r", encoding="utf-8") as handle:
            return [json.loads(line) for line in handle if line.strip()]

    def kinds(self):
        return [entry["kind"] for entry in self.lines()]

    def test_each_action_is_answered_before_the_next_is_asked_for(self):
        agent = self.agent(
            [
                {"expect": "task", "send": [{"type": "action", "code": CLICK}]},
                {"expect": "result", "send": [{"type": "action", "code": TYPE}]},
                {"expect": "result", "send": [finished()]},
            ]
        )

        _, first = agent.predict("do it", {})
        _, second = agent.predict("do it", {})
        _, third = agent.predict("do it", {})
        _, fourth = agent.predict("do it", {})

        self.assertIn("pyautogui.click(1, 2", first[0])
        self.assertIn("pyautogui.write('hi')", second[0])
        self.assertEqual(third, ["DONE"])
        self.assertEqual(fourth, ["DONE"], "the terminal action repeats")
        # The fake asserts the order itself; the log proves it ran to the end.
        self.assertIn("exit", self.kinds())
        self.assertNotIn("out-of-order", self.kinds())
        self.assertNotIn("unexpected", self.kinds())

        with open(os.path.join(self.dir, "result.json"), "r", encoding="utf-8") as handle:
            result = json.load(handle)
        self.assertEqual(result["answer"], "done")
        self.assertEqual(agent.last_result["stop_reason"], "final_answer")

    def test_the_command_line_and_the_environment_reach_the_child(self):
        agent = self.agent(
            [{"expect": "task", "send": [finished()]}],
            env={"SYSCITY_MAX_TOOL_ITERATIONS": "99"},
        )
        agent.predict("do it", {})

        entries = self.lines()
        argv = entries[0]["detail"]
        self.assertEqual(argv[0], "bench")
        self.assertEqual(argv[1], "serve")
        self.assertEqual(argv[argv.index("--computer-url") + 1], "http://10.0.0.5:5000")
        self.assertEqual(argv[argv.index("--max-steps") + 1], "7")
        recv = [entry["detail"] for entry in entries if entry["kind"] == "recv"]
        self.assertEqual(recv[0]["type"], "task")
        self.assertEqual(recv[0]["task_id"], "abc123")
        self.assertEqual(recv[0]["instruction"], "do it")
        # The flag is the budget; an inherited variable must not shadow it.
        self.assertIsNone(entries[1]["detail"])

    def test_no_action_before_the_first_turn_still_ends_cleanly(self):
        agent = self.agent([{"expect": "task", "send": [finished(answer="nothing to do")]}])
        _, actions = agent.predict("do it", {})
        self.assertEqual(actions, ["DONE"])

    def test_an_exhausted_budget_ends_in_fail_rather_than_done(self):
        agent = self.agent(
            [{"expect": "task", "send": [finished(answer="", stop_reason="max_steps")]}]
        )
        _, actions = agent.predict("do it", {})
        self.assertEqual(actions, ["FAIL"])

    def test_a_reported_error_is_an_infrastructure_failure_not_a_fail_action(self):
        agent = self.agent(
            [
                {"expect": "task", "send": [{"type": "action", "code": CLICK}]},
                {
                    "expect": "result",
                    "send": [{"type": "error", "schema": 1, "error": "provider 502"}],
                },
            ]
        )
        _, first = agent.predict("do it", {})
        self.assertIn("pyautogui.click", first[0])
        with self.assertRaises(SyscityRunFailed):
            agent.predict("do it", {})

    def test_an_agent_side_error_in_the_result_is_an_infrastructure_failure(self):
        agent = self.agent(
            [
                {
                    "expect": "task",
                    "send": [finished(answer="", stop_reason="error", error="provider 502")],
                }
            ]
        )
        with self.assertRaises(SyscityRunFailed):
            agent.predict("do it", {})
        self.assertTrue(
            os.path.exists(os.path.join(self.dir, "result.json")),
            "the result is kept even when it is an error",
        )

    def test_a_silent_child_is_a_timeout_not_a_hang(self):
        # It takes the task and then says nothing, which is what a wedged model
        # call looks like from here.
        agent = self.agent([{"expect": "task", "send": []}], timeout=0.5)
        started = time.monotonic()
        with self.assertRaises(SyscityRunFailed):
            agent.predict("do it", {})
        self.assertLess(time.monotonic() - started, 10.0, "the wait is bounded")


if __name__ == "__main__":
    unittest.main(verbosity=2)
