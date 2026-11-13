import copy
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from batch_metrics import aggregate, collect, graph_metrics, parallel_metrics, score

CONFIG = json.loads((Path(__file__).resolve().parents[1] / "scoring-v1.json").read_text())


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


if __name__ == "__main__":
    unittest.main()
