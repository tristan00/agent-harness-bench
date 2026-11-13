from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from template_session import can_still_run


class SessionFinish(unittest.TestCase):
    def test_only_tasks_that_can_still_run_keep_a_case_open(self):
        self.assertTrue(can_still_run({"stage": "implementation", "active": "attempt"}))
        self.assertTrue(can_still_run({"stage": "implementation", "active": None, "stopped": False, "blocked": False}))
        self.assertTrue(can_still_run({"stage": "review", "active": None}))
        self.assertTrue(can_still_run({"stage": "replan", "active": None}))
        self.assertFalse(can_still_run({"stage": "done", "active": None}))
        self.assertFalse(can_still_run({"stage": "implementation", "active": None, "stopped": True}))
        self.assertFalse(can_still_run({"stage": "replan", "active": None, "blocked": True}))
        self.assertFalse(can_still_run({"stage": "implementation", "active": None, "container": True}))


if __name__ == "__main__":
    unittest.main()
