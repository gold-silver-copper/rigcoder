"""Exercise the actual bounded prompt launcher with a mocked Gemini upstream."""
import http.client
import io
import json
from pathlib import Path
import sys
import tempfile
import threading
from unittest.mock import MagicMock, patch

from gemini_budget import Budget, cost
from gemini_gateway import server
from prompt_improve import launch


def check(binary):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        budget = Budget(root / "budget.sqlite")
        budget.initialize()
        gateway = server(("127.0.0.1", 0), budget, "proposal", "local-token", "fake-provider")
        thread = threading.Thread(target=gateway.serve_forever, daemon=True)
        thread.start()
        connection = MagicMock()
        responses = []
        for part in [{"functionCall": {"name": "write_file", "args": {
            "path": "crates/rigcoder/src/prompt.md", "content": "candidate prompt"}}}, {"text": "Proposal ready."}]:
            payload = ("data: " + json.dumps({"candidates": [{"content": {"role": "model", "parts": [part]},
                        "finishReason": "STOP", "index": 0}], "usageMetadata": {
                        "promptTokenCount": 10, "candidatesTokenCount": 3, "totalTokenCount": 13}}) + "\n\n").encode()
            class Socket:
                def makefile(self, *_args):
                    return io.BytesIO(f"HTTP/1.1 200 OK\r\nContent-Length: {len(payload)}\r\n\r\n".encode() + payload)
            response = http.client.HTTPResponse(Socket())
            response.begin()
            responses.append(response)
        connection.getresponse.side_effect = responses
        try:
            with patch("gemini_dispatch.http.client.HTTPSConnection", return_value=connection):
                proposal = launch(binary, b"baseline prompt", b"synthetic development evidence",
                                  gateway.server_port, "local-token", root / "evidence", timeout=20)
            assert proposal["prompt"] == b"candidate prompt", proposal
            assert budget.committed_microdollars() == 2 * cost(10, 3)
            manifest = json.loads((root / "evidence/manifest.json").read_text())
            assert manifest["status"] == "proposal_collected"
            assert manifest["candidate_prompt_sha256"] != manifest["baseline_prompt_sha256"]
        finally:
            gateway.shutdown()
            gateway.server_close()
            thread.join()
    print("PASS: bounded sandboxed prompt edit, collection and trusted usage settlement")


if __name__ == "__main__":
    check(sys.argv[1])
