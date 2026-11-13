import copy
import os
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
os.environ["AGENT_FAMILY"] = "codex"
from batch_metrics import aggregate, collect, graph_metrics, parallel_metrics, score

CONFIG = json.loads((Path(__file__).resolve().parents[1] / "scoring.json").read_text())


def task(name, **relations):
    return {"id":name, "acceptance":["works"], **relations}


class MetricsTests(unittest.TestCase):
    def test_failure_before_run_creation_scores_zero_without_agent_activity(self):
        case = {"workspace":"w", "benchmark":"chess960", "settings":{}, "error":"launch failed"}
        metrics = collect(case, None, None, None)
        result = score(metrics, None, CONFIG, "agent_seconds_per_development_second")
        self.assertEqual(result["score"],0)
        self.assertEqual(result["status"],"no_submission")
        self.assertEqual(metrics["evidence"]["error"],"launch failed")

    def test_relation_types_connectivity_and_external_edges(self):
        isolated = graph_metrics([task("a"), task("b"), task("c")])
        connected = graph_metrics([task("a", after=["b"], dependencies=[{"target":"c","reason":"uses_data"}], related=["b"]), task("b", related=["a"]), task("c")])
        self.assertEqual(isolated["orphans"], 3)
        self.assertEqual(connected["orphans"], 0)
        self.assertEqual(connected["near_orphans"], 2)
        self.assertEqual(connected["edge_count"], 3)
        self.assertEqual(connected["distinct_relation_types"], 3)
        self.assertEqual(graph_metrics([task("a", after=["outside"])])["external_edges_excluded"], 1)
        replaced = graph_metrics([task("old"), task("new", history=[{"kind":"created", "evidence":{"supersedes":["old"]}}])])
        self.assertEqual(replaced["relation_types"], {"supersedes":1})

    def test_overlap_is_computed_from_intervals(self):
        serial = parallel_metrics([("a",0,1000),("b",1000,2000)])
        parallel = parallel_metrics([("a",0,1000),("b",0,1000)])
        self.assertEqual(serial["parallel_fraction"], 0)
        self.assertEqual(parallel["parallel_fraction"], 1)
        self.assertEqual(parallel["effective_parallelism"], 2)
        self.assertEqual(parallel["worker_agent_seconds"], serial["worker_agent_seconds"])
        duplicate = parallel_metrics([("a",0,1000),("a",0,1000)])
        self.assertEqual(duplicate["effective_parallelism"],1)
        self.assertEqual(duplicate["parallel_fraction"],0)
        self.assertEqual(duplicate["worker_agent_seconds"],2)

    def test_weights_monotonicity_and_missing_submissions(self):
        metrics = {"run_id":"r", "workspace":"w", "benchmark":"game_build", "artifact":{"id":"a"}, "source_lines":1000,
                   "graph":graph_metrics([task("a"),task("b",after=["a"])]),
                   "parallel":parallel_metrics([("a",0,1000),("b",0,1000)]),
                   "tokens":{"recorded_total":100000,"proxy_agent_seconds":2,"development_seconds":1}}
        first = score(metrics, {"mean_out_of_5":3}, CONFIG, "measured_tokens")
        self.assertEqual(first["categories"]["result"],60)
        self.assertEqual(first["categories"]["simplicity"],50)
        self.assertAlmostEqual(first["score"],sum(first["categories"].values())/3)
        longer = copy.deepcopy(metrics); longer["source_lines"] *= 2; longer["tokens"]["recorded_total"] *= 2
        second = score(longer, {"mean_out_of_5":3}, CONFIG, "measured_tokens")
        self.assertLess(second["score"], first["score"])
        metrics["artifact"] = None; metrics["source_lines"] = None
        missing = score(metrics, None, CONFIG, "measured_tokens")
        self.assertEqual(missing["categories"]["simplicity"],0)
        self.assertEqual(missing["categories"]["result"],0)
        cases = [{**first,"benchmark":name,"score":value} for name,value in zip(CONFIG["benchmark_weights"],[20,40,60,80])]
        self.assertEqual(aggregate(cases,CONFIG)[0]["score"],50)

    def maze_metrics(self, lines=3000, tokens=600000):
        return {"run_id":"m", "workspace":"w", "benchmark":"maze_game", "artifact":{"id":"a"}, "source_lines":lines,
                "graph":graph_metrics([task("a"),task("b",after=["a"])]),
                "parallel":parallel_metrics([("a",0,1000),("b",0,1000)]),
                "tokens":{"recorded_total":tokens,"proxy_agent_seconds":2,"development_seconds":1}}

    def report(self, completed, mean):
        return {"mean_out_of_5":mean, "progress":{"fraction_sum":completed, "requests_received":6, "requests_total":6}}

    def test_maze_result_splits_completion_and_quality_evenly(self):
        full = score(self.maze_metrics(), self.report(6, 5), CONFIG, "measured_tokens")
        self.assertAlmostEqual(full["categories"]["result"], 100)
        half = score(self.maze_metrics(), self.report(3, 2.5), CONFIG, "measured_tokens")
        self.assertAlmostEqual(half["categories"]["result"], 50)
        quality_only = score(self.maze_metrics(), self.report(0, 5), CONFIG, "measured_tokens")
        self.assertAlmostEqual(quality_only["categories"]["result"], 50)

    def test_maze_simplicity_and_tokens_are_normalized_per_completed_request(self):
        far = score(self.maze_metrics(3000, 600000), self.report(6, 3), CONFIG, "measured_tokens")
        stuck = score(self.maze_metrics(500, 100000), self.report(1, 3), CONFIG, "measured_tokens")
        self.assertAlmostEqual(far["categories"]["simplicity"], stuck["categories"]["simplicity"])
        self.assertAlmostEqual(far["framework_components"]["token_efficiency"], stuck["framework_components"]["token_efficiency"])
        self.assertEqual(far["per_request_normalization"], {"requests_completed": 6, "divisor": 6})
        barely = score(self.maze_metrics(500, 100000), self.report(0.25, 3), CONFIG, "measured_tokens")
        self.assertEqual(barely["per_request_normalization"]["divisor"], 1)
        other = score({**self.maze_metrics(), "benchmark":"game_build"}, {"mean_out_of_5":3}, CONFIG, "measured_tokens")
        self.assertIsNone(other["per_request_normalization"])
        self.assertEqual(other["categories"]["simplicity"], 25)

    def test_scoring_weights_all_four_benchmarks(self):
        from score_batch import validate_config
        validate_config(CONFIG)
        broken = copy.deepcopy(CONFIG); del broken["maze"]
        with self.assertRaises(KeyError):
            validate_config(broken)
        first = score(self.maze_metrics(), self.report(6, 5), CONFIG, "measured_tokens")
        cases = [{**first, "benchmark":name, "score":value} for name, value in zip(CONFIG["benchmark_weights"], [20, 40, 60, 80])]
        self.assertEqual(aggregate(cases, CONFIG)[0]["score"], 50)

    def test_report_contributions_reconstruct_scores_without_hidden_weights(self):
        config = copy.deepcopy(CONFIG)
        config["category_weights"] = {"result": 3, "simplicity": 2, "framework": 1}
        config["framework_weights"] = {"task_count": 1, "relations": 2, "parallelism": 3, "token_efficiency": 4}
        result = score(self.maze_metrics(), self.report(3, 2.5), config, "agent_seconds_per_development_second")
        trace = result["calculation"]
        for node in [trace["benchmark"], trace["framework"], trace["result"], trace["framework_components"]["relations"]["aggregation"]]:
            terms = node["terms"].values()
            self.assertAlmostEqual(sum(term["contribution"] for term in terms), node["value"])
            self.assertAlmostEqual(sum(term["normalized_weight"] for term in terms), 1)
            self.assertAlmostEqual(node["weighted_sum"] / node["weight_sum"], node["value"])
        self.assertEqual(trace["benchmark"]["value"], result["score"])
        self.assertEqual(trace["simplicity"]["inputs"]["normalized_lines"], 1000)
        self.assertAlmostEqual(trace["framework_components"]["token_efficiency"]["cost"]["value"], 2 / 3)
        cases = [{**result, "benchmark":name} for name in config["benchmark_weights"]]
        row = aggregate(cases, config)[0]
        self.assertAlmostEqual(sum(term["contribution"] for term in row["calculation"]["terms"].values()), row["score"])
        json.dumps(row, allow_nan=False)

    def test_report_exposes_chess_and_each_judge_rating(self):
        metrics = {**self.maze_metrics(), "benchmark":"chess960"}
        result = score(metrics, {"metrics":{"elo_change":100}}, CONFIG, "measured_tokens")
        self.assertEqual(result["calculation"]["result"]["inputs"]["elo_change"], 100)
        native = {"mean_out_of_5":3, "scores":{"visuals":{"score":4,"evidence":"Observed scene"}, "interface":{"score":2,"evidence":"Observed controls"}}}
        result = score({**metrics,"benchmark":"game_build"}, native, CONFIG, "measured_tokens")
        trace = result["calculation"]["result"]
        self.assertEqual(trace["criteria"], native["scores"])
        self.assertEqual(trace["mean"]["value"], 3)
        self.assertEqual(trace["value"], 60)

    def test_classification_is_the_fill_rate_of_every_tracked_attribute(self):
        filled = {"languages":["python"],"functions":["api"],"priority":1}
        tasks = [task("a", attributes=dict(filled), dependencies=[{"target":"b","reason":"uses_api"}]),
                 task("b", attributes=dict(filled), dependencies=[{"target":"a","reason":"uses_data"}])]
        metrics = self.maze_metrics()
        metrics["graph"] = graph_metrics(tasks)
        full = score(metrics, self.report(3, 3), CONFIG, "measured_tokens")
        self.assertEqual(full["framework_components"]["classification"], 100)
        tasks[1]["attributes"]["priority"] = None
        metrics["graph"] = graph_metrics(tasks)
        one_missing = score(metrics, self.report(3, 3), CONFIG, "measured_tokens")
        trace = one_missing["calculation"]["framework_components"]["classification"]
        self.assertAlmostEqual(one_missing["framework_components"]["classification"], 100 * 5 / 6)
        self.assertEqual(trace["inputs"]["filled_slots"], 5)
        self.assertEqual(trace["inputs"]["total_slots"], 6)
        self.assertEqual(trace["inputs"]["missing_by_attribute"]["priority"], ["b"])
        tasks[0]["attributes"] = {"languages":[],"functions":["api"]}
        tasks[0]["dependencies"][0]["reason"] = None
        metrics["graph"] = graph_metrics(tasks)
        self.assertAlmostEqual(score(metrics, self.report(3, 3), CONFIG, "measured_tokens")["framework_components"]["classification"], 100 * 3 / 6 * .5)
        empty = collect({"workspace":"w","benchmark":"maze_game","settings":{}}, None, Path("."), Path("."))
        self.assertEqual(score(empty, None, CONFIG, "measured_tokens")["framework_components"]["classification"], 0)

    def test_untrackable_classification_attributes_are_rejected(self):
        from score_batch import validate_config
        validate_config(CONFIG)
        bad = copy.deepcopy(CONFIG)
        bad["classification_attributes"] = ["languages", "lane"]
        with self.assertRaises(ValueError):
            validate_config(bad)

    def test_source_line_breakdown_matches_the_frozen_artifact(self):
        import base64
        import tempfile
        from batch_metrics import source_line_details
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "submissions/a"
            target.mkdir(parents=True)
            (target / "submission.json").write_text(json.dumps({"files":[
                {"path":"a.py","text":"# comment\r\n\r\nprint(1)\n"},
                {"path":"b.rs","base64":base64.b64encode(b"fn main() {}\n").decode()},
                {"path":"README.md","text":"Not counted"}]}))
            artifact = {"id":"a","source_files":["a.py","b.rs"],"lines_of_code":3}
            self.assertEqual(source_line_details(root, artifact), {"a.py":2,"b.rs":1})
            artifact["lines_of_code"] = 4
            with self.assertRaises(ValueError):
                source_line_details(root, artifact)


if __name__ == "__main__":
    unittest.main()
