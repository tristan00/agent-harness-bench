import os
import sys
import tempfile
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import agent_selection as selection


class AgentSelection(unittest.TestCase):
    def test_launcher_family_detection(self):
        detect = selection.detect_family
        self.assertEqual(detect({"CLAUDECODE": "1"}), "claude")
        self.assertEqual(detect({"CODEX_WINDOWS_REGISTERED_CORE": "x"}), "codex")
        self.assertIsNone(detect({"CLAUDECODE": "1", "CODEX_THREAD_ID": "x"}))
        self.assertIsNone(detect({"CODEX_HOME": "x"}))
        self.assertEqual(detect({"AGENT_FAMILY": "Codex", "CLAUDECODE": "1"}), "codex")
        self.assertEqual(detect({"AGENT_FAMILY": "", "CLAUDECODE": "1"}), "claude")
        self.assertIsNone(detect({"AGENT_FAMILY": "both"}))

    def test_catalog_holds_only_hosted_planner_and_judge_agents(self):
        for name, agent in selection.CATALOG.items():
            self.assertIn((agent["provider"], agent["harness"]), selection.PAIRS, name)
            self.assertEqual(set(agent) - {"provider", "harness", "model", "effort", "retired"}, set(), name)

    def test_vendors_never_cross(self):
        for name, agent in selection.CATALOG.items():
            if agent.get("retired"):
                with self.assertRaises(ValueError):
                    selection.resolve(name, "codex")
                continue
            for family in ("claude", "codex", ""):
                if selection.VENDOR[agent["provider"]] == family:
                    self.assertEqual(selection.resolve(name, family)["model"], agent["model"])
                else:
                    with self.assertRaises(ValueError):
                        selection.resolve(name, family)

    def test_only_the_planner_is_checked_in_workspace_settings(self):
        previous = os.environ.get("AGENT_FAMILY")
        os.environ["AGENT_FAMILY"] = "codex"
        try:
            settings = {"planner": "sol_6_1_light", "worker": "not validated here"}
            self.assertIs(selection.workspace_settings(settings), settings)
            for planner in ("luna_6_high", "claude_haiku_5_5_low", None):
                with self.assertRaises(ValueError):
                    selection.workspace_settings({"planner": planner, "worker": "sol_6_1_light"})
            self.assertEqual(selection.command_arguments("sol_6_1_light"), ["--model", "gpt-6.1-sol", "-c", 'model_reasoning_effort="low"'])
        finally:
            if previous is None:
                os.environ.pop("AGENT_FAMILY", None)
            else:
                os.environ["AGENT_FAMILY"] = previous

    def test_claude_launch_strips_host_session_and_sets_model_effort(self):
        env = {"CLAUDECODE": "1", "CLAUDE_CODE_SESSION_ID": "s", "CLAUDE_AGENT_SDK_VERSION": "1", "PATH": "p", "ANTHROPIC_API_KEY": "k"}
        self.assertEqual(selection.claude_environment(env), {"PATH": "p", "ANTHROPIC_API_KEY": "k"})
        argv = selection.claude_arguments({"model": "claude-haiku-5-5", "effort": "low"})
        self.assertEqual(argv[argv.index("--model") + 1], "claude-haiku-5-5")
        self.assertEqual(argv[argv.index("--effort") + 1], "low")

    def test_npm_shim_resolves_to_native_claude_executable(self):
        with tempfile.TemporaryDirectory() as root:
            shim = Path(root, "claude.cmd")
            shim.write_text("")
            self.assertEqual(selection.claude_executable(str(shim)), str(shim))
            native = Path(root, "node_modules", "@anthropic-ai", "claude-code", "bin", "claude.exe")
            native.parent.mkdir(parents=True)
            native.write_text("")
            self.assertEqual(selection.claude_executable(str(shim)), str(native))


if __name__ == "__main__":
    unittest.main()
