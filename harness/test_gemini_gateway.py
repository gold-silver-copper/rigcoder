import http.client
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import MagicMock, patch

from gemini_budget import Budget, MAX_INPUT, MAX_OUTPUT, cost
from gemini_dispatch import MODEL
from gemini_gateway import server


class GatewayTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.budget = Budget(Path(self.temp.name) / "budget.sqlite")
        self.budget.initialize()
        self.server = server(("127.0.0.1", 0), self.budget, "proposal", "local-token", "upstream-secret")
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.stop)
        self.path = f"/v1beta/models/{MODEL}:generateContent"

    def stop(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def request(self, token="local-token", path=None, headers=None):
        connection = http.client.HTTPConnection(*self.server.server_address, timeout=5)
        try:
            connection.request("POST", path or self.path,
                               b'{"contents":[{"parts":[{"text":"hello"}]}]}',
                               headers=headers if headers is not None else {"x-goog-api-key": token})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def test_auth_path_and_framing_rejected_before_dispatch(self):
        with patch("gemini_gateway.send") as dispatch:
            self.assertEqual(self.request(token="wrong")[0], 401)
            self.assertEqual(self.request(path=self.path + "?key=secret")[0], 401)
            self.assertEqual(self.request(headers={"x-goog-api-key": "local-token",
                                                  "Transfer-Encoding": "chunked"})[0], 400)
            dispatch.assert_not_called()
        self.assertEqual(self.budget.committed_microdollars(), 0)

    def test_query_auth_and_ambiguous_targets(self):
        with patch("gemini_gateway.send", return_value=(200, "application/json", b"{}")) as dispatch:
            self.assertEqual(self.request(path=self.path + "?key=local-token", headers={})[0], 200)
            dispatch.assert_called_once()
            dispatch.reset_mock()
            for suffix in ("?key=local-token&key=local-token", "?key=local-token&extra=1",
                           "?key=local-token&alt=sse"):
                self.assertGreaterEqual(self.request(path=self.path + suffix, headers={})[0], 400)
            self.assertEqual(self.request(path=self.path + "?key=local-token")[0], 401)
            dispatch.assert_not_called()

    def test_http_to_dispatch_reserves_and_replaces_credentials(self):
        connection = MagicMock()
        response = connection.getresponse.return_value
        response.status = 200
        response.read.return_value = b'{"candidates":[]}'
        response.getheader.return_value = "application/json"
        with patch("gemini_dispatch.http.client.HTTPSConnection", return_value=connection) as upstream:
            self.assertEqual(self.request(), (200, b'{"candidates":[]}'))
            args, kwargs = connection.request.call_args
            self.assertEqual(args, ("POST", self.path))
            self.assertEqual(kwargs["headers"]["x-goog-api-key"], "upstream-secret")
            self.assertEqual(self.budget.committed_microdollars(), cost(MAX_INPUT, MAX_OUTPUT))
            # Another HTTP request cannot evade this server's fixed phase cap.
            status, body = self.request()
            self.assertEqual(status, 502)
            self.assertNotIn(b"upstream-secret", body)
            upstream.assert_called_once()


if __name__ == "__main__":
    unittest.main()
