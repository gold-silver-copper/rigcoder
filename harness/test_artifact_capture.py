import sys
import time
import unittest
from unittest.mock import patch

from artifact_capture import bounded_command, capture
from artifact_score import MAX_ARCHIVE


class CaptureTests(unittest.TestCase):
    def test_real_subprocess_output_and_wall_bounds(self):
        self.assertEqual(bounded_command([sys.executable, "-c", "print('data')"], 16, 5), b"data\n")
        for script in ("print('x' * 100)", "import sys; sys.stderr.write('x' * 70000)"):
            with self.assertRaisesRegex(ValueError, "limit"):
                bounded_command([sys.executable, "-c", script], 16, 5)
        start = time.monotonic()
        with self.assertRaisesRegex(ValueError, "timed out"):
            bounded_command([sys.executable, "-c", "import time; time.sleep(20)"], 16, 0.2)
        self.assertLess(time.monotonic() - start, 5)
        with self.assertRaisesRegex(ValueError, "failed"):
            bounded_command([sys.executable, "-c", "raise SystemExit(2)"], 16, 5)

    def test_missing_artifact_requires_a_surviving_stopped_container(self):
        missing = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
        with patch("artifact_capture.bounded_command", side_effect=[b"ok", b"false", missing, b"false"]):
            self.assertIsNone(capture("candidate", "/app/answer.txt"))
        with patch("artifact_capture.bounded_command", side_effect=[b"ok", b"false", missing, ValueError("gone")]):
            with self.assertRaises(ValueError):
                capture("candidate", "/app/answer.txt")
        broken = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"
        with patch("artifact_capture.bounded_command", side_effect=[b"ok", b"false", broken]):
            with self.assertRaises(ValueError):
                capture("candidate", "/app/answer.txt")

    def test_truncated_http_responses_are_not_scores(self):
        for status in (b"200 OK", b"404 Not Found"):
            raw = b"HTTP/1.1 " + status + b"\r\nContent-Length: 100\r\n\r\n"
            with patch("artifact_capture.bounded_command", side_effect=[b"ok", b"false", raw]):
                with self.assertRaisesRegex(ValueError, "invalid archive response"):
                    capture("candidate", "/app/answer.txt")

    def test_request_pipe_is_bounded_by_the_same_deadline(self):
        request = b"test request"
        self.assertEqual(bounded_command([sys.executable, "-c", "import sys; sys.stdout.buffer.write(sys.stdin.buffer.read())"],
                                         100, 5, request), request)
        with self.assertRaisesRegex(ValueError, "timed out"):
            bounded_command([sys.executable, "-c", "import time; time.sleep(20)"],
                            16, 0.2, b"x" * 1_000_000)

    def test_stop_is_confirmed_before_archive_capture(self):
        with patch("artifact_capture.bounded_command", side_effect=[b"candidate", b"false\n", b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\ntar", b"false\n"]) as command:
            self.assertEqual(capture("candidate", "/app/answer.txt"), b"tar")
            self.assertEqual([call.args[0][1] for call in command.call_args_list], ["stop", "inspect", "system", "inspect"])
            self.assertEqual(command.call_args_list[2].args[1], MAX_ARCHIVE + 65_536)
        with patch("artifact_capture.bounded_command", side_effect=[b"candidate", b"true\n"]) as command:
            with self.assertRaisesRegex(ValueError, "still running"):
                capture("candidate", "/app/answer.txt")
            self.assertEqual(command.call_count, 2)
        with patch("artifact_capture.bounded_command") as command:
            for container, artifact in [("--help", "/app/answer"), ("candidate", "/app/../secret"),
                                        ("candidate", "relative")]:
                with self.assertRaises(ValueError):
                    capture(container, artifact)
            command.assert_not_called()


if __name__ == "__main__":
    unittest.main()
