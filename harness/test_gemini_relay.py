import http.client
import io
import json
from pathlib import Path
import socket
import struct
import tempfile
import threading
import time
import unittest
from unittest.mock import MagicMock, patch

from gemini_budget import Budget, cost
from gemini_dispatch import MODEL
from gemini_relay import REQUEST_LIMIT, endpoint, read_frame, serve, write_frame


class RelayTests(unittest.TestCase):
    def test_invalid_pipe_request_cannot_reach_dispatch(self):
        for frame in (b'\x02{}', b'\x01'):
            with self.subTest(frame=frame):
                left, right = socket.socketpair()
                try:
                    right.sendall(struct.pack("!I", len(frame)) + frame)
                    with patch("gemini_relay.send") as dispatch:
                        with self.assertRaisesRegex(ValueError, "invalid relay request"):
                            serve(left.fileno(), left.fileno(), None, "development", "secret", time.monotonic() + 1)
                        dispatch.assert_not_called()
                finally:
                    left.close()
                    right.close()

    def test_real_http_and_pipe_relay_uses_host_budget_and_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            budget = Budget(Path(directory) / "budget.sqlite")
            budget.initialize()
            host, remote = socket.socketpair()
            deadline = time.monotonic() + 10
            gateway = endpoint(remote.fileno(), remote.fileno(), 0, "local-token", deadline)
            failures = []

            def host_loop():
                try:
                    serve(host.fileno(), host.fileno(), budget, "holdout", "HOST_SECRET", deadline)
                except Exception as error:
                    failures.append(error)

            worker = threading.Thread(target=host_loop, daemon=True)
            listener = threading.Thread(target=gateway.serve_forever, daemon=True)
            worker.start()
            listener.start()
            payload = json.dumps({"candidates": [{"index": 0, "finishReason": "STOP",
                "content": {"role": "model", "parts": [{"text": "answer"}]}}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3,
                                  "totalTokenCount": 13}}).encode()

            class ResponseSocket:
                def makefile(self, *_args):
                    return io.BytesIO(f"HTTP/1.1 200 OK\r\nContent-Length: {len(payload)}\r\n\r\n".encode() + payload)

            response = http.client.HTTPResponse(ResponseSocket())
            response.begin()
            connection = MagicMock()
            connection.getresponse.return_value = response
            client = http.client.HTTPConnection("127.0.0.1", gateway.server_port, timeout=5)
            try:
                with patch("gemini_dispatch.http.client.HTTPSConnection", return_value=connection):
                    client.request("POST", f"/v1beta/models/{MODEL}:generateContent?key=local-token",
                                   json.dumps({"contents": [{"parts": [{"text": "hello"}]}]}))
                    result = client.getresponse()
                    self.assertEqual(result.status, 200)
                    self.assertEqual(result.read(), payload)
                self.assertEqual(budget.committed_microdollars(), cost(10, 3))
                self.assertEqual(connection.request.call_args.kwargs["headers"]["x-goog-api-key"], "HOST_SECRET")
                with budget._transaction() as db:
                    self.assertEqual(db.execute("SELECT phase FROM reservations").fetchall(), [("holdout",)])
                client.request("POST", f"/v1beta/models/{MODEL}:generateContent?key=local-token", b'{"unsupported":true}')
                rejected = client.getresponse()
                self.assertEqual(rejected.status, 502)
                self.assertNotIn(b"HOST_SECRET", rejected.read())
                self.assertEqual(budget.committed_microdollars(), cost(10, 3))
            finally:
                client.close()
                gateway.shutdown()
                gateway.server_close()
                listener.join(timeout=2)
                remote.shutdown(socket.SHUT_RDWR)
                worker.join(timeout=2)
                remote.close()
                host.close()
            self.assertFalse(worker.is_alive())
            self.assertEqual(failures, [])

    def test_frame_limits_truncation_and_deadlines(self):
        for prefix in (struct.pack("!I", REQUEST_LIMIT + 1), struct.pack("!I", 0), b'\x00\x00'):
            with self.subTest(prefix=prefix):
                left, right = socket.socketpair()
                try:
                    right.sendall(prefix)
                    right.shutdown(socket.SHUT_WR)
                    with self.assertRaises(ValueError):
                        read_frame(left.fileno(), REQUEST_LIMIT, time.monotonic() + 1)
                finally:
                    left.close()
                    right.close()
        left, right = socket.socketpair()
        try:
            with self.assertRaises(TimeoutError):
                read_frame(left.fileno(), REQUEST_LIMIT, time.monotonic() + 0.05)
            left.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
            with self.assertRaises(TimeoutError):
                write_frame(left.fileno(), b'x' * 1_000_000, REQUEST_LIMIT, time.monotonic() + 0.05)
        finally:
            left.close()
            right.close()


if __name__ == "__main__":
    unittest.main()
