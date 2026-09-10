from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest

from improver_sandbox import command


@unittest.skipUnless(sys.platform == "darwin", "macOS sandbox backend")
class SandboxTests(unittest.TestCase):
    def test_hidden_files_and_symlink_escape_are_denied(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root).resolve()
            workspace = root / "workspace"
            workspace.mkdir()
            hidden = root / "hidden"
            hidden.write_text("SYNTHETIC_HOLDOUT\n")
            (workspace / "escape").symlink_to(hidden)
            script = 'printf "edited\\n" > allowed; if IFS= read -r value < "$1"; then exit 10; fi; if IFS= read -r value < escape; then exit 11; fi; if printf overwritten > "$1"; then exit 12; fi'
            result = subprocess.run(command("/bin/bash", workspace, 12345) + ["-c", script, "sh", str(hidden)],
                                    cwd=workspace, env={"PATH": "/usr/bin:/bin", "HOME": str(workspace)},
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((workspace / "allowed").read_text(), "edited\n")
            self.assertNotIn("SYNTHETIC_HOLDOUT", result.stdout)
            self.assertEqual(hidden.read_text(), "SYNTHETIC_HOLDOUT\n")

    def test_forks_and_other_executables_are_denied(self):
        with tempfile.TemporaryDirectory() as workspace:
            for script in ("(printf forked)", "exec /usr/bin/true"):
                result = subprocess.run(command("/bin/bash", workspace, 12345) + ["-c", script],
                                        cwd=workspace, env={"PATH": "/usr/bin:/bin"},
                                        capture_output=True, timeout=10)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn(b"forked", result.stdout)

    def test_only_the_designated_gateway_port_is_reachable(self):
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"gateway")
            def log_message(self, *_args):
                pass
        servers = [ThreadingHTTPServer(("127.0.0.1", 0), Handler) for _ in range(2)]
        threads = [threading.Thread(target=server.serve_forever, daemon=True) for server in servers]
        for thread in threads:
            thread.start()
        try:
            with tempfile.TemporaryDirectory() as workspace:
                argv = command("/usr/bin/curl", workspace, servers[0].server_port)
                for i, server in enumerate(servers):
                    result = subprocess.run(argv + ["--silent", "--show-error", "--max-time", "2", f"http://127.0.0.1:{server.server_port}"],
                                            cwd=workspace, env={"PATH": "/usr/bin:/bin"}, capture_output=True, timeout=5)
                    if i == 0:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(result.stdout, b"gateway")
                    else:
                        self.assertNotEqual(result.returncode, 0)
        finally:
            for server in servers:
                server.shutdown()
                server.server_close()
            for thread in threads:
                thread.join()


if __name__ == "__main__":
    unittest.main()
