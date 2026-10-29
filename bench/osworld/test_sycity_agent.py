#!/usr/bin/env python3
"""Tests for the replay agent, with a scripted stand-in for the syscity binary.

Run with no OSWorld checkout and no Rust build:

    python3 bench/osworld/test_sycity_agent.py

The point is the join, not the model: that the command line names the VM, that
the snippets come back in order, that a syscity-side error is raised as an
infrastructure failure instead of a `FAIL` action, and that infrastructure
errors stay out of the reported denominator.
"""

import json
import os
import stat
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from sycity_agent import SyscityAgent, SyscityRunFailed, summarize  # noqa: E402

FAKE_SYSCITY = """#!/usr/bin/env python3
import json, sys

out = sys.argv[sys.argv.index("--out") + 1]
payload = json.loads({payload!r})
payload["argv"] = sys.argv[1:]
with open(out, "w", encoding="utf-8") as handle:
    json.dump(payload, handle)
sys.exit({exit_code})
"""


def write_fake_syscity(directory, payload, exit_code=0):
    path = os.path.join(directory, "fake-syscity")
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(FAKE_SYSCITY.format(payload=json.dumps(payload), exit_code=exit_code))
    os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)
    return path


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


if __name__ == "__main__":
    unittest.main(verbosity=2)
