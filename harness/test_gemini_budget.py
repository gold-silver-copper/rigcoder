import concurrent.futures
from contextlib import closing
from pathlib import Path
import sqlite3
import tempfile
import unittest

from gemini_budget import Budget, MAX_INPUT, MAX_OUTPUT, cost


class BudgetTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "budget.sqlite"
        self.budget = Budget(self.path)
        self.budget.initialize()

    def test_concurrent_reservations_and_restart_cannot_overspend(self):
        def reserve(_):
            try:
                return Budget(self.path).reserve("proposal", MAX_INPUT, MAX_OUTPUT)
            except ValueError:
                return None
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(reserve, range(16)))
        self.assertEqual(sum(value is not None for value in results), 1)
        self.assertEqual(Budget(self.path).committed_microdollars(), cost(MAX_INPUT, MAX_OUTPUT))
        with self.assertRaises(FileExistsError):
            self.budget.initialize()

    def test_unknown_usage_stays_reserved_and_settlement_is_once(self):
        reservation = self.budget.reserve("development", MAX_INPUT, MAX_OUTPUT)
        held = self.budget.committed_microdollars()
        with self.assertRaises(ValueError):
            self.budget.settle(reservation, None, 0)
        self.assertEqual(self.budget.committed_microdollars(), held)
        self.budget.settle(reservation, 1, 1)
        self.assertEqual(self.budget.committed_microdollars(), 5)
        with self.assertRaises(ValueError):
            self.budget.settle(reservation, 0, 0)
        with self.assertRaises(ValueError):
            self.budget.settle(999, 0, 0)

    def test_missing_corrupt_and_modified_ledgers_fail_closed(self):
        with self.assertRaises(FileNotFoundError):
            Budget(self.path.with_name("missing")).reserve("proposal", 1, 1)
        with closing(sqlite3.connect(self.path)) as db:
            with db:
                db.execute("UPDATE limits SET amount = 999999999")
        with self.assertRaises(ValueError):
            self.budget.reserve("proposal", 1, 1)
        self.path.write_bytes(b"corrupt")
        with self.assertRaises(sqlite3.DatabaseError):
            self.budget.reserve("proposal", 1, 1)

    def test_invalid_bounds_and_excess_usage_preserve_reservation(self):
        for bad in [-1, True, 1.5, MAX_INPUT + 1]:
            with self.assertRaises(ValueError):
                self.budget.reserve("development", bad, 1)
        reservation = self.budget.reserve("development", 1, 1)
        with self.assertRaises(ValueError):
            self.budget.settle(reservation, 10, 10)
        self.assertEqual(self.budget.committed_microdollars(), 45)
        with self.assertRaisesRegex(ValueError, "stopped"):
            Budget(self.path).reserve("development", 1, 1)


if __name__ == "__main__":
    unittest.main()
