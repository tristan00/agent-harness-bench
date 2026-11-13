import sys
import tempfile
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import agent_selection as selection
import local_agent


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

    def test_vendors_never_cross_and_local_is_always_allowed(self):
        for name, agent in selection.CATALOG.items():
            if agent.get("retired"):
                with self.assertRaises(ValueError):
                    selection.resolve(name, "codex")
                continue
            for family in ("claude", "codex", ""):
                expected = agent["provider"] == "local" or selection.VENDOR[agent["provider"]] == family
                if expected:
                    self.assertEqual(selection.resolve(name, family)["model"], agent["model"])
                else:
                    with self.assertRaises(ValueError):
                        selection.resolve(name, family)

    def test_workspace_definitions_are_validated(self):
        defined = {"tiny": {"provider": "local", "harness": "openai_compatible", "model": "t", "endpoint": "http://127.0.0.1:9/v1"}}
        self.assertEqual(selection.resolve("tiny", "claude", defined)["model"], "t")
        for broken in ({**defined["tiny"], "endpoint": "https://host/v1"},
                       {"provider": "openai", "harness": "claude_code", "model": "x"},
                       {**defined["tiny"], "model": " "}):
            with self.assertRaises(ValueError):
                selection.resolve("tiny", "codex", {"tiny": broken})
        settings = {"planner": "tiny", "worker": "local_qwen3_4b", "agents": defined}
        self.assertEqual(set(selection.agent_definitions(settings)), {"tiny", "local_qwen3_4b"})

    def test_codex_arguments_only_for_codex_agents(self):
        selection_family = selection.detect_family
        selection.detect_family = lambda environ=None: "codex"
        try:
            self.assertEqual(selection.command_arguments("sol_6_1_light"), ["--model", "gpt-6.1-sol", "-c", 'model_reasoning_effort="low"'])
            with self.assertRaises(ValueError):
                selection.command_arguments("local_qwen3_4b")
        finally:
            selection.detect_family = selection_family

    def test_claude_launch_strips_host_session_and_sets_model_effort(self):
        env = {"CLAUDECODE": "1", "CLAUDE_CODE_SESSION_ID": "s", "CLAUDE_AGENT_SDK_VERSION": "1", "PATH": "p", "ANTHROPIC_API_KEY": "k"}
        self.assertEqual(selection.claude_environment(env), {"PATH": "p", "ANTHROPIC_API_KEY": "k"})
        argv = selection.claude_arguments({"model": "claude-haiku-5-5", "effort": "low"}, read_only=True)
        self.assertEqual(argv[argv.index("--model") + 1], "claude-haiku-5-5")
        self.assertEqual(argv[argv.index("--effort") + 1], "low")
        self.assertIn("--disallowedTools", argv)


    def test_npm_shim_resolves_to_native_claude_executable(self):
        with tempfile.TemporaryDirectory() as root:
            shim = Path(root, "claude.cmd")
            shim.write_text("")
            self.assertEqual(selection.claude_executable(str(shim)), str(shim))
            native = Path(root, "node_modules", "@anthropic-ai", "claude-code", "bin", "claude.exe")
            native.parent.mkdir(parents=True)
            native.write_text("")
            self.assertEqual(selection.claude_executable(str(shim)), str(native))


class LocalAgent(unittest.TestCase):
    def test_loop_runs_tools_until_final_and_counts_usage(self):
        replies = iter(['{"tool":"write_file","args":{"path":"a.txt","content":"hi"}}', "not json",
                        '{"tool":"read_file","args":{"path":"a.txt"}}', '{"final":"done"}'])
        sent, events = [], []

        def send(endpoint, body):
            sent.append([dict(message) for message in body["messages"]])
            return {"choices": [{"message": {"content": next(replies)}}], "usage": {"prompt_tokens": 10, "completion_tokens": 2}}
        with tempfile.TemporaryDirectory() as cwd:
            code = local_agent.main("http://127.0.0.1:1/v1", "m", "do it", cwd, 10, events.append, send=send)
            self.assertEqual(Path(cwd, "a.txt").read_text(), "hi")
        self.assertEqual(code, 0)
        self.assertEqual(events[-1]["final"], "done")
        self.assertEqual(events[-1]["usage"], {"input_tokens": 40, "output_tokens": 8, "turns": 4})
        self.assertIn("reply with one JSON object", sent[2][-1]["content"])
        self.assertIn("hi", sent[3][-1]["content"])

    def test_loop_stops_at_step_limit(self):
        events = []
        send = lambda endpoint, body: {"choices": [{"message": {"content": '{"tool":"list_files","args":{}}'}}]}
        with tempfile.TemporaryDirectory() as cwd:
            self.assertEqual(local_agent.main("e", "m", "p", cwd, 3, events.append, send=send), 1)
        self.assertIn("error", events[-1])

    def test_tools_stay_in_working_directory_and_report_commands(self):
        with tempfile.TemporaryDirectory() as root:
            cwd = Path(root, "work")
            cwd.mkdir()
            self.assertTrue(local_agent.run_tool({"tool": "write_file", "args": {"path": "../x.txt", "content": "x"}}, cwd).startswith("error"))
            self.assertFalse(Path(root, "x.txt").exists())
            ran = local_agent.run_tool({"tool": "run_command", "args": {"argv": [sys.executable, "-c", "print(5)"]}}, cwd)
            self.assertTrue(ran.startswith("exit 0") and "5" in ran)
            self.assertTrue(local_agent.run_tool({"tool": "run_command", "args": {"argv": "ls"}}, cwd).startswith("error"))
            self.assertTrue(local_agent.run_tool({"tool": "other"}, cwd).startswith("error"))
        self.assertIn("truncated", local_agent.clip("x" * 20000))


if __name__ == "__main__":
    unittest.main()
