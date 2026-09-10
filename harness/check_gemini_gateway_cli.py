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


def check(binary):
    binary = str(Path(binary).resolve(strict=True))
    with tempfile.TemporaryDirectory() as directory:
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
        class Socket:
            def makefile(self, *_args):
                headers = f"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {len(payload)}\r\n\r\n"
                return io.BytesIO(headers.encode() + payload)
        response = http.client.HTTPResponse(Socket())
        response.begin()
        connection.getresponse.return_value = response
        try:
            with patch('gemini_dispatch.http.client.HTTPSConnection', return_value=connection) as upstream:
                result = subprocess.run([
                    binary, '--provider', 'gemini', '--model', 'gemini-3.8-flash',
                    '--gemini-gateway', f'http://127.0.0.1:{gateway.server_port}',
                    '-C', directory, '--max-turns', '1', '--timeout-secs', '10',
                    'Say hello without using tools.'
                ], env={'PATH': '/usr/bin:/bin', 'RIGCODER_GATEWAY_TOKEN': 'local-token'},
                    capture_output=True, text=True, timeout=20)
                if result.returncode or 'Gateway verified.' not in result.stdout:
                    raise AssertionError(f'CLI failed: {result.returncode}\n{result.stdout}\n{result.stderr}')
                upstream.assert_called_once()
                assert connection.request.call_args.kwargs['headers']['x-goog-api-key'] == 'fake-upstream'
                assert budget.committed_microdollars() == cost(10, 3)
        finally:
            gateway.shutdown()
            gateway.server_close()
            thread.join()
    print('PASS: actual CLI -> local gateway -> mocked upstream; settlement and credential replacement verified')


if __name__ == '__main__':
    check(sys.argv[1])
