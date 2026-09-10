"""Launcher lifecycle checks with real subprocesses and no provider requests."""
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from prompt_improve import launch
from prompt_workspace import PROMPT


class LauncherTests(unittest.TestCase):
    def exercise(self, script, root, timeout=5):
        # Replace only sandbox construction; exercise the real resource wrapper,
        # bounded subprocess capture, environment, collection and manifest.
        with patch("prompt_improve.command", return_value=[sys.executable, "-c", script]):
            return launch(sys.executable, b"baseline", b"development", 12345,
                          "synthetic-token", root / "evidence", timeout=timeout)

    def test_environment_and_copyback_are_explicit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = (
                "import os; from pathlib import Path; "
                "assert 'SEALED_HOST_VALUE' not in os.environ; "
                "assert os.environ['RIGCODER_GATEWAY_TOKEN'] == 'synthetic-token'; "
                "assert Path.cwd() == Path(os.environ['HOME']); "
                f"Path({str(PROMPT)!r}).write_text('candidate'); "
                "Path('extra').write_text('ignored')"
            )
            with patch.dict("os.environ", {"SEALED_HOST_VALUE": "secret"}):
                result = self.exercise(script, root)
            self.assertEqual(result, {"prompt": b"candidate", "note": None})
            manifest = json.loads((root / "evidence/manifest.json").read_text())
            self.assertEqual(manifest["status"], "proposal_collected")

    def test_failed_timed_out_and_invalid_proposals_preserve_failure_evidence(self):
        scripts = {
            "exit": "raise SystemExit(7)",
            "timeout": "import time; time.sleep(30)",
            "invalid_utf8": f"from pathlib import Path; Path({str(PROMPT)!r}).write_bytes(b'\\xff')",
            "unsafe_note": "from pathlib import Path; Path('improvement.md').symlink_to('development.md')",
        }
        for kind, script in scripts.items():
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                with self.assertRaises((ValueError, OSError)):
                    self.exercise(script, root, timeout=1 if kind == "timeout" else 5)
                manifest = json.loads((root / "evidence/manifest.json").read_text())
                self.assertEqual(manifest["status"], "failed")
                self.assertNotIn("candidate_prompt_sha256", manifest)
                self.assertTrue((root / "evidence/workspace/development.md").is_file())


if __name__ == "__main__":
    unittest.main()
