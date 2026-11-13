import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
os.environ.setdefault("AGENT_FAMILY", "codex")
import ml_eval
from batch_metrics import graph_metrics, parallel_metrics, score

CONFIG = json.loads((Path(__file__).resolve().parents[1] / "scoring.json").read_text())


class MlEvaluation(unittest.TestCase):
    def test_roc_auc_handles_ranks_and_ties(self):
        self.assertAlmostEqual(ml_eval.roc_auc([0, 0, 1, 1], [0.1, 0.4, 0.35, 0.8]), 0.75)
        self.assertAlmostEqual(ml_eval.roc_auc([0, 1, 0, 1], [0.5, 0.5, 0.5, 0.5]), 0.5)
        with self.assertRaises(ValueError):
            ml_eval.roc_auc([1, 1], [0.2, 0.3])

    def test_scores_are_scaled_between_trivial_reference_and_perfect(self):
        self.assertEqual(ml_eval.normalized(None, 0.9, 0.5), 0)
        self.assertEqual(ml_eval.normalized(0.4, 0.9, 0.5), 0)
        self.assertAlmostEqual(ml_eval.normalized(0.7, 0.9, 0.5), 25)
        self.assertAlmostEqual(ml_eval.normalized(0.9, 0.9, 0.5), 50)
        self.assertAlmostEqual(ml_eval.normalized(0.95, 0.9, 0.5), 75)
        self.assertAlmostEqual(ml_eval.normalized(1.0, 0.9, 0.5), 100)
        with self.assertRaises(ValueError):
            ml_eval.metric("accuracy", [1, 2], ["1"])

    def test_hidden_inputs_drop_labels(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "adult.test").write_text("|1x3 Cross validator\n39, Private, x, >50K.\n25, State-gov, y, <=50K.\n\n")
            features, labels = ml_eval.test_split("adult_income", {"adult.test": root / "adult.test"}, root / "adult")
            self.assertEqual(features.read_text().splitlines(), ["39, Private, x", "25, State-gov, y"])
            self.assertEqual(labels, [1, 0])
            (root / "optdigits.tes").write_text("0,1,2,7\n3,4,5,1\n")
            features, labels = ml_eval.test_split("optical_digits", {"optdigits.tes": root / "optdigits.tes"}, root / "digits")
            self.assertEqual(features.read_text().splitlines(), ["0,1,2", "3,4,5"])
            self.assertEqual(labels, [7, 1])

    def test_predict_command_runs_on_hidden_data_with_the_configured_python(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            hidden = root / "optdigits.tes"
            hidden.write_text("0,1,7\n3,4,1\n5,6,7\n")
            script = "import sys\nrows = open(sys.argv[1]).read().split()\nopen(sys.argv[2], 'w').write('\\n'.join('7' for _ in rows))\n"
            submission = {"manifest": {"predict": ["python", "predict.py", "{input}", "{output}"]}, "files": [{"path": "predict.py", "text": script}]}
            (root / "data" / "submissions" / "s").mkdir(parents=True)
            (root / "data" / "submissions" / "s" / "submission.json").write_text(json.dumps(submission))
            entry = {"id": "optical_digits", "metric": "accuracy", "reference": {"score": 0.9, "trivial": 0.1, "model": "fixture"}}
            run = {"id": "r", "submission_id": "s", "ml": {"task": {"id": "optical_digits"}}}
            config = {"ml_python": sys.executable, "predict_seconds": 30}
            with mock.patch.object(ml_eval, "registry", return_value={"optical_digits": entry}), \
                 mock.patch.object(ml_eval, "fetch_test", return_value={"optdigits.tes": hidden}):
                result = ml_eval.evaluate(run, config, root / "data", root / "out")
            self.assertIsNone(result["error"], result)
            self.assertAlmostEqual(result["score"], 2 / 3)
            self.assertAlmostEqual(result["normalized"], 50 * (2 / 3 - 0.1) / 0.8)
            submission["manifest"]["predict"] = ["python", "missing.py", "{input}", "{output}"]
            (root / "data" / "submissions" / "s" / "submission.json").write_text(json.dumps(submission))
            run["id"] = "broken"
            with mock.patch.object(ml_eval, "registry", return_value={"optical_digits": entry}), \
                 mock.patch.object(ml_eval, "fetch_test", return_value={"optdigits.tes": hidden}):
                broken = ml_eval.evaluate(run, config, root / "data", root / "out")
            self.assertIsNotNone(broken["error"])
            self.assertEqual(broken["normalized"], 0)

    def test_ml_result_averages_eda_and_model(self):
        metrics = {"run_id": "m", "workspace": "w", "benchmark": "ml_predictor", "artifact": {"id": "a"}, "source_lines": 100,
                   "graph": graph_metrics([]), "parallel": parallel_metrics([]),
                   "tokens": {"recorded_total": 1, "proxy_agent_seconds": 1, "development_seconds": 1}}
        report = {"mean_out_of_5": 4, "scores": {}, "model_evaluation": {"normalized": 50, "score": 0.9}}
        result = score(metrics, report, CONFIG, "measured_tokens")
        self.assertAlmostEqual(result["categories"]["result"], 65)
        self.assertEqual(result["calculation"]["result"]["terms"]["model"]["value"], 50)
        missing = score(metrics, {"mean_out_of_5": 4, "scores": {}}, CONFIG, "measured_tokens")
        self.assertAlmostEqual(missing["categories"]["result"], 40)


if __name__ == "__main__":
    unittest.main()
