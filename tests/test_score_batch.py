import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
os.environ["AGENT_FAMILY"] = "codex"
from score_batch import judge_command, output_lock, review_case, validate_config

CONFIG = json.loads((Path(__file__).resolve().parents[1] / "scoring-v1.json").read_text())


class RunnerTests(unittest.TestCase):
    def test_duplicate_commands_cannot_claim_the_same_output(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "lock"
            with output_lock(path):
                with self.assertRaises(RuntimeError):
                    with output_lock(path):
                        self.fail("second command acquired the lock")
            with output_lock(path):
                pass

    def test_reviewer_cannot_inherit_model_effort_or_browser_session(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "prompt.txt"
            path.write_text("Only the assigned submission", encoding="utf-8")
            command = judge_command("sol_light", path, path.with_name("answer.txt"), "0.0.85")
            self.assertEqual(command[command.index("--model") + 1], "gpt-6-sol")
            self.assertIn('model_reasoning_effort="low"', command)
            self.assertIn("--ephemeral", command)
            options = next(arg for arg in command if arg.startswith("mcp_servers.playwright.args="))
            browser = json.loads(options.split("=", 1)[1])
            self.assertIn("--isolated", browser)
            self.assertIn("--headless", browser)
            self.assertIn("@playwright/mcp@0.0.85", browser)
            for selection in [None, "astra_xhigh", "sol_medium"]:
                with self.assertRaises(ValueError):
                    judge_command(selection, path, path.with_name("answer.txt"), "0.0.85")

    def test_existing_scores_and_missing_submissions_never_launch_judges(self):
        case = {"run_id": "test"}
        client = Mock()
        client.request.return_value = {"report": {"mean_out_of_5": 3}}
        self.assertEqual(review_case(None, case, client, None, CONFIG), {"mean_out_of_5": 3})
        client.request.assert_called_once_with("GET", "/v1/runs/test")
        client.reset_mock()
        client.request.return_value = {"report": None, "run": {"submission_id": None}}
        self.assertIsNone(review_case(None, case, client, None, CONFIG))
        client.request.assert_called_once_with("GET", "/v1/runs/test")

    def test_configuration_rejects_undefined_and_nonfinite_scoring(self):
        validate_config(CONFIG)
        for field, key, value in [("category_weights", "result", 0), ("references", "tokens", float("nan")),
                                  ("framework_weights", "task_count", -1)]:
            config = copy.deepcopy(CONFIG)
            config[field][key] = value
            with self.assertRaises(ValueError):
                validate_config(config)
        config = copy.deepcopy(CONFIG)
        config["judge_selection"] = "astra_xhigh"
        with self.assertRaises(ValueError):
            validate_config(config)


if __name__ == "__main__":
    unittest.main()
