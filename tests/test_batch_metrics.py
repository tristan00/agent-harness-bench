import copy
import os
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
os.environ["AGENT_FAMILY"] = "codex"
from batch_metrics import aggregate, collect, graph_metrics, parallel_metrics, score

CONFIG = json.loads((Path(__file__).resolve().parents[1] / "scoring-v1.json").read_text())
CONFIG_V2 = json.loads((Path(__file__).resolve().parents[1] / "scoring-v2.json").read_text())


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
        cases = [{**first,"benchmark":name,"score":value} for name,value in zip(CONFIG["benchmark_weights"],[30,60,90])]
        self.assertEqual(aggregate(cases,CONFIG)[0]["score"],60)

    def maze_metrics(self, lines=3000, tokens=600000):
        return {"run_id":"m", "workspace":"w", "benchmark":"maze_game", "artifact":{"id":"a"}, "source_lines":lines,
                "graph":graph_metrics([task("a"),task("b",after=["a"])]),
                "parallel":parallel_metrics([("a",0,1000),("b",0,1000)]),
                "tokens":{"recorded_total":tokens,"proxy_agent_seconds":2,"development_seconds":1}}

    def report(self, completed, mean):
        return {"mean_out_of_5":mean, "progress":{"fraction_sum":completed, "requests_received":6, "requests_total":6}}

    def test_maze_result_splits_completion_and_quality_evenly(self):
        full = score(self.maze_metrics(), self.report(6, 5), CONFIG_V2, "measured_tokens")
        self.assertAlmostEqual(full["categories"]["result"], 100)
        half = score(self.maze_metrics(), self.report(3, 2.5), CONFIG_V2, "measured_tokens")
        self.assertAlmostEqual(half["categories"]["result"], 50)
        quality_only = score(self.maze_metrics(), self.report(0, 5), CONFIG_V2, "measured_tokens")
        self.assertAlmostEqual(quality_only["categories"]["result"], 50)

    def test_maze_simplicity_and_tokens_are_normalized_per_completed_request(self):
        far = score(self.maze_metrics(3000, 600000), self.report(6, 3), CONFIG_V2, "measured_tokens")
        stuck = score(self.maze_metrics(500, 100000), self.report(1, 3), CONFIG_V2, "measured_tokens")
        self.assertAlmostEqual(far["categories"]["simplicity"], stuck["categories"]["simplicity"])
        self.assertAlmostEqual(far["framework_components"]["token_efficiency"], stuck["framework_components"]["token_efficiency"])
        self.assertEqual(far["per_request_normalization"], {"requests_completed": 6, "divisor": 6})
        barely = score(self.maze_metrics(500, 100000), self.report(0.25, 3), CONFIG_V2, "measured_tokens")
        self.assertEqual(barely["per_request_normalization"]["divisor"], 1)
        other = score({**self.maze_metrics(), "benchmark":"game_build"}, {"mean_out_of_5":3}, CONFIG_V2, "measured_tokens")
        self.assertIsNone(other["per_request_normalization"])
        self.assertEqual(other["categories"]["simplicity"], 25)

    def test_scoring_v2_weights_all_four_benchmarks(self):
        from score_batch import validate_config
        validate_config(CONFIG)
        validate_config(CONFIG_V2)
        broken = copy.deepcopy(CONFIG_V2); del broken["maze"]
        with self.assertRaises(KeyError):
            validate_config(broken)
        first = score(self.maze_metrics(), self.report(6, 5), CONFIG_V2, "measured_tokens")
        cases = [{**first, "benchmark":name, "score":value} for name, value in zip(CONFIG_V2["benchmark_weights"], [20, 40, 60, 80])]
        self.assertEqual(aggregate(cases, CONFIG_V2)[0]["score"], 50)


if __name__ == "__main__":
    unittest.main()
