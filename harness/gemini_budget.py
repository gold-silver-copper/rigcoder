"""Trusted ledger for the bounded Gemini 3.8 Flash experiment.

Amounts are integer microdollars. This ledger must live outside candidate access.
The transport must reserve before each send and enforce the supplied token bounds;
this module alone does not constrain requests or network access.
"""

from contextlib import contextmanager
from datetime import datetime, timezone
import os
from pathlib import Path
import sqlite3
import stat

LIMITS = {"proposal": 2_000_000, "development": 10_000_000, "holdout": 6_000_000}
TOTAL = 18_000_000  # $2 headroom below the user's $20 hard cap.
MAX_INPUT = 1_048_576
MAX_OUTPUT = 65_536


def cost(input_tokens, output_tokens):
    """Round up standard-tier $0.75/M input and $3.75/M total output."""
    for value, maximum in [(input_tokens, MAX_INPUT), (output_tokens, MAX_OUTPUT)]:
        if type(value) is not int or not 0 <= value <= maximum:
            raise ValueError("invalid token bound")
    return (3 * input_tokens + 15 * output_tokens + 3) // 4


class Budget:
    def __init__(self, path, context=None):
        self.path = Path(path).absolute()
        if context is not None and (not isinstance(context, str) or not 0 < len(context.encode("utf-8")) <= 4096 or "\x00" in context):
            raise ValueError("invalid budget context")
        self.context = context

    def initialize(self):
        """Create a new ledger explicitly; never reset an existing budget."""
        fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        os.close(fd)
        # An interrupted initialization leaves an unusable ledger, not a reset.
        with self._transaction(check=False) as db:
            db.execute("CREATE TABLE limits (phase TEXT PRIMARY KEY, amount INTEGER NOT NULL)")
            db.executemany("INSERT INTO limits VALUES (?, ?)", LIMITS.items())
            db.execute("CREATE TABLE reservations (id INTEGER PRIMARY KEY, phase TEXT NOT NULL, reserved INTEGER NOT NULL, actual INTEGER, context TEXT)")
            db.execute("CREATE TABLE state (stopped INTEGER NOT NULL CHECK (stopped IN (0, 1)))")
            db.execute("INSERT INTO state VALUES (0)")
            db.execute("PRAGMA user_version = 2")

    @contextmanager
    def _transaction(self, check=True):
        if not stat.S_ISREG(self.path.lstat().st_mode):
            raise ValueError("budget must be a regular file")
        db = sqlite3.connect(self.path.as_uri() + "?mode=rw", uri=True, timeout=30)
        try:
            db.execute("PRAGMA synchronous = FULL")
            db.execute("BEGIN IMMEDIATE")
            if check:
                if db.execute("PRAGMA user_version").fetchone()[0] != 2:
                    raise ValueError("unknown budget schema")
                if dict(db.execute("SELECT phase, amount FROM limits")) != LIMITS:
                    raise ValueError("budget limits changed")
            yield db
            db.commit()
        except BaseException:
            db.rollback()
            raise
        finally:
            db.close()

    def reserve(self, phase, input_bound, output_bound):
        if datetime.now(timezone.utc).date().isoformat() > "2026-12-31":
            raise ValueError("Gemini price authorization expired; revalidate rates")
        if phase not in LIMITS:
            raise ValueError("unknown experiment phase")
        amount = cost(input_bound, output_bound)
        if amount == 0 or output_bound == 0:
            raise ValueError("generation requires a positive reservation and output bound")
        with self._transaction() as db:
            if db.execute("SELECT stopped FROM state").fetchall() != [(0,)]:
                raise ValueError("budget stopped after a reservation violation")
            total, selected = db.execute(
                "SELECT COALESCE(SUM(COALESCE(actual, reserved)), 0), "
                "COALESCE(SUM(CASE WHEN phase = ? THEN COALESCE(actual, reserved) ELSE 0 END), 0) "
                "FROM reservations", (phase,)
            ).fetchone()
            if total + amount > TOTAL or selected + amount > LIMITS[phase]:
                raise ValueError("experiment budget exhausted")
            reservation = db.execute(
                "INSERT INTO reservations (phase, reserved, context) VALUES (?, ?, ?)", (phase, amount, self.context)
            ).lastrowid
        return reservation  # The durable commit precedes permission to send.

    def settle(self, reservation, input_tokens, output_tokens):
        """Only trusted complete usage may replace a reservation; unknown stays held."""
        amount = cost(input_tokens, output_tokens)
        with self._transaction() as db:
            row = db.execute("SELECT reserved, actual FROM reservations WHERE id = ? AND context IS ?", (reservation, self.context)).fetchone()
            if row is None or row[1] is not None:
                raise ValueError("unknown or settled request")
            exceeded = amount > row[0]
            db.execute("UPDATE reservations SET actual = ? WHERE id = ?", (amount, reservation))
            if exceeded:
                db.execute("UPDATE state SET stopped = 1")
        # Commit the fault and known charge before surfacing the error.
        if exceeded:
            raise ValueError("usage exceeded reservation; budget permanently stopped")

    def committed_microdollars(self):
        with self._transaction() as db:
            return db.execute("SELECT COALESCE(SUM(COALESCE(actual, reserved)), 0) FROM reservations").fetchone()[0]

    def accounting(self):
        """Context totals; unresolved requests retain their full reservation."""
        with self._transaction() as db:
            requests, unsettled, known, held = db.execute(
                "SELECT COUNT(*), COALESCE(SUM(actual IS NULL), 0), "
                "COALESCE(SUM(actual), 0), "
                "COALESCE(SUM(CASE WHEN actual IS NULL THEN reserved ELSE 0 END), 0) "
                "FROM reservations WHERE context IS ?", (self.context,)
            ).fetchone()
        return {"requests": requests, "unsettled_requests": unsettled,
                "known_cost_microdollars": known, "held_microdollars": held}
