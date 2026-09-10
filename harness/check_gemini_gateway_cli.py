"""Offline actual-binary gateway check: python3 -B harness/check_gemini_gateway_cli.py BINARY."""
import json
import io
import http.client
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from unittest.mock import MagicMock, patch

from gemini_budget import Budget, cost
from gemini_gateway import server


def check(binary, isolated=False):
    binary = str(Path(binary).resolve(strict=True))
    with tempfile.TemporaryDirectory() as directory:
        workspace = Path(directory) / 'workspace'
        workspace.mkdir()
        budget = Budget(Path(directory) / 'budget.sqlite')
        budget.initialize()
        gateway = server(('127.0.0.1', 0), budget, 'development', 'local-token', 'fake-upstream')
        thread = threading.Thread(target=gateway.serve_forever, daemon=True)
        thread.start()
        connection = MagicMock()
        payload = ('data: ' + json.dumps({
            'candidates': [{'content': {'role': 'model', 'parts': [{'text': 'Gateway verified.'}]},
                            'finishReason': 'STOP', 'index': 0}],
            'usageMetadata': {'promptTokenCount': 10, 'candidatesTokenCount': 3, 'totalTokenCount': 13}
        }) + '\n\n').encode()
        payloads = [payload]
        hidden = Path(directory) / "hidden-canary"
        hidden.write_text("SYNTHETIC_SEALED_VALUE")
        if isolated:
            tool = {"candidates": [{"content": {"role": "model", "parts": [{"functionCall": {
                "name": "read_file", "args": {"path": str(hidden)}}}]}, "finishReason": "STOP", "index": 0}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3, "totalTokenCount": 13}}
            payloads.insert(0, ("data: " + json.dumps(tool) + "\n\n").encode())

        def response_for(body):
            class Socket:
                def makefile(self, *_args):
                    headers = f"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {len(body)}\r\n\r\n"
                    return io.BytesIO(headers.encode() + body)
            response = http.client.HTTPResponse(Socket())
            response.begin()
            return response
        connection.getresponse.side_effect = [response_for(body) for body in payloads]
        try:
            with patch('gemini_dispatch.http.client.HTTPSConnection', return_value=connection) as upstream:
                argv = [binary]
                if isolated:
                    from improver_sandbox import command
                    argv = command(binary, workspace, gateway.server_port)
                result = subprocess.run(argv + [
                    '--provider', 'gemini', '--model', 'gemini-3.8-flash',
                    '--gemini-gateway', f'http://127.0.0.1:{gateway.server_port}',
                    '-C', str(workspace), '--max-turns', str(len(payloads)), '--timeout-secs', '10',
                    'Say hello without using tools.'
                ], env={'PATH': '/usr/bin:/bin', 'HOME': str(workspace), 'TMPDIR': str(workspace),
                           'RIGCODER_GATEWAY_TOKEN': 'local-token'},
                    capture_output=True, text=True, timeout=20)
                if result.returncode or 'Gateway verified.' not in result.stdout:
                    raise AssertionError(f'CLI failed: {result.returncode}\n{result.stdout}\n{result.stderr}')
                assert upstream.call_count == len(payloads)
                if isolated:
                    second = connection.request.call_args_list[1].kwargs["body"]
                    assert b"functionResponse" in second
                    assert b"SYNTHETIC_SEALED_VALUE" not in second
                    assert b"Operation not permitted" in second
                    assert "SYNTHETIC_SEALED_VALUE" not in result.stdout
                assert connection.request.call_args.kwargs['headers']['x-goog-api-key'] == 'fake-upstream'
                assert budget.committed_microdollars() == len(payloads) * cost(10, 3)
        finally:
            gateway.shutdown()
            gateway.server_close()
            thread.join()
    print('PASS: actual CLI -> local gateway -> mocked upstream; settlement and credential replacement verified')


if __name__ == '__main__':
    check(sys.argv[1], "--sandbox" in sys.argv[2:])
