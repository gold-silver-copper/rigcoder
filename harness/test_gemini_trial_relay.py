import os
from pathlib import Path
import selectors
import subprocess
import sys
import tempfile
import unittest

from gemini_budget import Budget


class ControllerTests(unittest.TestCase):
    def test_parent_eof_deadline_and_cleanup_failure_stop_the_relay(self):
        for mode in ("parent_eof", "deadline", "stop_failure", "late_request"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                budget = Budget(root / "budget.sqlite")
                budget.initialize()
                docker = root / "docker"
                docker.write_text(f"#!{sys.executable}\n" + r'''
import os, pathlib, signal, struct, sys, time
root = pathlib.Path(os.environ['RELAY_TEST_ROOT'])
assert 'HOST_SECRET' not in ' '.join(sys.argv)
if sys.argv[1] == 'exec':
    (root / 'pid').write_text(str(os.getpid()))
    os.write(1, struct.pack('!I', 5) + b'ready')
    time.sleep(60)
elif sys.argv[1] == 'stop':
    (root / 'stopped').write_text('yes')
    if os.environ['RELAY_TEST_MODE'] == 'late_request':
        deadline = time.monotonic() + 5
        while not (root / 'admission').exists():
            if time.monotonic() > deadline: sys.exit(1)
            time.sleep(0.01)
    if os.environ['RELAY_TEST_MODE'] == 'stop_failure': sys.exit(1)
    os.kill(int((root / 'pid').read_text()), signal.SIGKILL)
else:
    sys.exit(1)
''')
                docker.chmod(0o755)
                harness = Path(__file__).resolve().parent
                script = f"""
import sys
sys.path.insert(0, {str(harness)!r})
from pathlib import Path
from gemini_budget import Budget
from gemini_trial_relay import MODULES, entry
import gemini_trial_relay, os, time
if os.environ['RELAY_TEST_MODE'] == 'late_request':
    def late_request(_incoming, _outgoing, budget, phase, _key, _deadline):
        root = Path(os.environ['RELAY_TEST_ROOT'])
        while not (root / 'stopped').exists(): time.sleep(0.01)
        try:
            budget.reserve(phase, 10, 3)
        except ValueError:
            (root / 'admission').write_text('rejected')
        else:
            (root / 'admission').write_text('accepted')
    gemini_trial_relay.serve = late_request
sources = {{name: (Path({str(harness)!r}) / (name + '.py')).read_text() for name in MODULES}}
raise SystemExit(entry(sources))
"""
                process = subprocess.Popen([sys.executable, "-I", "-c", script,
                    "synthetic-container", str(budget.path), "development", "1" if mode == "deadline" else "20", "synthetic-trial"],
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    env={**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                         "RELAY_TEST_ROOT": str(root), "RELAY_TEST_MODE": mode,
                         "GEMINI_API_KEY": "HOST_SECRET"})
                try:
                    with selectors.DefaultSelector() as selector:
                        selector.register(process.stdout, selectors.EVENT_READ)
                        self.assertTrue(selector.select(5), "controller readiness timed out")
                    self.assertEqual(process.stdout.readline(), b"ready\n")
                    if mode == "deadline":
                        process.wait(timeout=10)
                    stdout, stderr = process.communicate(timeout=10)
                    self.assertEqual(process.returncode == 0, mode in ("parent_eof", "late_request"), stderr.decode())
                    self.assertTrue((root / "stopped").is_file())
                    self.assertNotIn(b"HOST_SECRET", stdout + stderr)
                    self.assertEqual(budget.committed_microdollars(), 0)
                    if mode == "late_request":
                        self.assertEqual((root / "admission").read_text(), "rejected")
                    with self.assertRaises(ProcessLookupError):
                        os.kill(int((root / "pid").read_text()), 0)
                finally:
                    if process.poll() is None:
                        process.kill()
                        process.wait(timeout=5)
                    process.stdin.close()
                    process.stdout.close()
                    process.stderr.close()


if __name__ == "__main__":
    unittest.main()
