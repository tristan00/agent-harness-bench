import json
import os
import sys
import tempfile
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
os.environ["AGENT_FAMILY"] = "codex"
import template_planner
import template_session

CODEX = {"provider": "openai", "harness": "codex", "model": "gpt-6.1-sol", "effort": "low"}
CLAUDE = {"provider": "anthropic", "harness": "claude_code", "model": "claude-haiku-5-5", "effort": "low"}


class Supervisor(unittest.TestCase):
    def test_clean_exit_with_a_new_submission_hands_off(self):
        self.assertEqual(template_session.next_step(0, {"open": True, "submitted": True}, 2), ("advance", 0))

    def test_exits_without_progress_repeat_the_request_until_the_stall_limit(self):
        state = {"open": True, "submitted": False}
        self.assertEqual(template_session.next_step(0, state, 0), ("continue", 1))
        self.assertEqual(template_session.next_step(1, {"open": True, "submitted": True}, 1), ("continue", 2))
        self.assertEqual(template_session.next_step(0, state, 2), ("stop", 3))

    def test_closed_runs_finish_regardless_of_exit(self):
        self.assertEqual(template_session.next_step(0, {"open": False, "submitted": True}, 0)[0], "finished")


    def test_only_active_tasks_inside_the_workspace_block_the_handoff(self):
        class Coordinator:
            def request(self, method, path):
                return {"tasks": [{"active": "a1", "cwd": "D:\\work\\ws1\\maze_game"}, {"active": "a2", "cwd": "D:\\work\\ws2\\maze_game"},
                                  {"active": None, "cwd": "D:\\work\\ws1\\maze_game"}]}
        found = template_session.active_tasks(Coordinator(), "D:\\work\\ws1\\maze_game")
        self.assertEqual([task["active"] for task in found], ["a1"])


class PlannerCommands(unittest.TestCase):
    def test_codex_runs_ephemeral_unless_resumable_and_resumes_by_thread(self):
        argv, _, capture = template_planner.plan("sol_6_1_light", CODEX, False, {}, None)
        self.assertIn("--ephemeral", argv)
        self.assertFalse(capture)
        argv, _, capture = template_planner.plan("sol_6_1_light", CODEX, True, {}, None)
        self.assertNotIn("--ephemeral", argv)
        self.assertTrue(capture)
        argv, _, capture = template_planner.plan("sol_6_1_light", CODEX, True, {"session": "thread-1"}, b"next")
        self.assertEqual(argv[1:3], ["exec", "resume"])
        self.assertEqual(argv[-2:], ["thread-1", "-"])
        self.assertIn("gpt-6.1-sol", argv)
        self.assertFalse(capture)

    def test_claude_gets_a_fixed_session_then_resumes_it(self):
        argv, saved, _ = template_planner.plan("claude_haiku_5_5_low", CLAUDE, True, {}, None)
        self.assertEqual(argv[argv.index("--session-id") + 1], saved["session"])
        self.assertNotIn("--no-session-persistence", argv)
        argv, _, _ = template_planner.plan("claude_haiku_5_5_low", CLAUDE, True, saved, b"next")
        self.assertEqual(argv[argv.index("--resume") + 1], saved["session"])
        argv, _, _ = template_planner.plan("claude_haiku_5_5_low", CLAUDE, False, {}, None)
        self.assertIn("--no-session-persistence", argv)


if __name__ == "__main__":
    unittest.main()
