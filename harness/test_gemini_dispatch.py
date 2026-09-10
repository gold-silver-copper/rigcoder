import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, MagicMock

from gemini_budget import Budget, MAX_INPUT, MAX_OUTPUT, cost
from gemini_dispatch import send


class DispatchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.budget = Budget(Path(self.temp.name) / "budget.sqlite")
        self.budget.initialize()
        self.body = {"contents": [{"role": "user", "parts": [{"text": "test"}]}]}

    def test_reserve_before_send_and_no_retry_after_failure(self):
        with patch("gemini_dispatch.http.client.HTTPSConnection") as factory:
            def connect(*args, **kwargs):
                self.assertEqual(self.budget.committed_microdollars(), cost(MAX_INPUT, MAX_OUTPUT))
                raise OSError("offline failure")
            factory.side_effect = connect
            with self.assertRaises(OSError):
                send(self.budget, "proposal", json.dumps(self.body).encode(), "fake")
            with self.assertRaisesRegex(ValueError, "exhausted"):
                send(self.budget, "proposal", json.dumps(self.body).encode(), "fake")
            self.assertEqual(factory.call_count, 1)

    def test_paid_tools_and_multiple_candidates_never_reach_network(self):
        for extra in [{"tools": [{"googleSearch": {}}]},
                      {"tools": [{"functionDeclarations": [], "codeExecution": {}}]}, {"cachedContent": "hidden"},
                      {"generationConfig": {"candidateCount": 2}},
                      {"generationConfig": {"responseModalities": ["IMAGE"]}},
                      {"generationConfig": {"serviceTier": "priority"}}]:
            with patch("gemini_dispatch.http.client.HTTPSConnection") as factory:
                with self.assertRaises(ValueError):
                    send(self.budget, "development", json.dumps(self.body | extra).encode(), "fake")
                factory.assert_not_called()
        self.assertEqual(self.budget.committed_microdollars(), 0)

    def test_nested_function_response_media_is_rejected_before_reservation(self):
        body = {"contents": [{"role": "user", "parts": [{"functionResponse": {
            "name": "tool", "response": {"text": "ok"},
            "parts": [{"inlineData": {"mimeType": "image/png", "data": "AAAA"}}]
        }}]}]}
        with patch("gemini_dispatch.http.client.HTTPSConnection") as factory:
            with self.assertRaisesRegex(ValueError, "envelope"):
                send(self.budget, "development", json.dumps(body).encode(), "fake")
            factory.assert_not_called()
        self.assertEqual(self.budget.committed_microdollars(), 0)

    def test_redirect_is_returned_without_following_or_releasing_reservation(self):
        connection = MagicMock()
        response = connection.getresponse.return_value
        response.status = 307
        response.read.return_value = b"redirect"
        response.getheader.return_value = "text/plain"
        with patch("gemini_dispatch.http.client.HTTPSConnection", return_value=connection) as factory:
            result = send(self.budget, "development", json.dumps(self.body).encode(), "fake", stream=True)
        self.assertEqual(result, (307, "text/plain", b"redirect"))
        factory.assert_called_once()
        connection.request.assert_called_once()
        connection.close.assert_called_once()
        self.assertEqual(self.budget.committed_microdollars(), cost(MAX_INPUT, MAX_OUTPUT))
