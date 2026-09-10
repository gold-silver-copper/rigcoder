"""Real network-none Docker relay, mocked upstream, no provider charges."""
import http.client
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from unittest.mock import MagicMock, patch

from artifact_capture import bounded_command
from gemini_budget import Budget, cost
from gemini_relay import REQUEST_LIMIT, read_frame, serve


def check():
    def docker(*args, timeout=30):
        return bounded_command(["docker", *args], 65_536, timeout)

    docker("pull", "python:3.13-slim", timeout=120)
    image = docker("image", "inspect", "--format", "{{.Id}}", "python:3.13-slim").decode().strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
        raise ValueError("invalid relay image identity")
    container = "rigcoder-relay-check-" + uuid.uuid4().hex
    script = "import sys, types, time\n"
    for name in ("gemini_budget", "gemini_usage", "gemini_dispatch", "gemini_gateway", "gemini_relay"):
        source = Path(__file__).with_name(name + ".py").read_text()
        script += f"m = types.ModuleType({name!r}); sys.modules[{name!r}] = m\nexec({source!r}, m.__dict__)\n"
    script += """
from gemini_relay import endpoint, write_frame, REQUEST_LIMIT
deadline = time.monotonic() + 60
gateway = endpoint(0, 1, 18080, 'local-token', deadline)
write_frame(1, b'ready', REQUEST_LIMIT, deadline)
gateway.serve_forever()
"""
    process = None
    worker = None
    failures = []
    removed = False
    try:
        docker("run", "-d", "--name", container, "--network", "none", "--read-only",
               "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
               "--memory", "128m", "--cpus", "1", "--pids-limit", "32",
               image, "python3", "-c", "import time; time.sleep(3600)")
        with tempfile.TemporaryDirectory() as directory:
            budget = Budget(Path(directory) / "budget.sqlite")
            budget.initialize()
            process = subprocess.Popen(["docker", "exec", "-i", container, "python3", "-I", "-c", script],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            deadline = time.monotonic() + 60
            assert read_frame(process.stdout.fileno(), REQUEST_LIMIT, deadline) == b"ready"

            def dispatch():
                try:
                    serve(process.stdout.fileno(), process.stdin.fileno(), budget,
                          "development", "HOST_ONLY_FAKE_KEY", deadline)
                except Exception as error:
                    failures.append(error)

            worker = threading.Thread(target=dispatch, daemon=True)
            worker.start()
            payload = json.dumps({"candidates": [{"index": 0, "finishReason": "STOP",
                "content": {"parts": [{"text": "answer"}]}}], "usageMetadata": {
                "promptTokenCount": 10, "candidatesTokenCount": 3, "totalTokenCount": 13}}).encode()

            class ResponseSocket:
                def makefile(self, *_args):
                    return io.BytesIO(f"HTTP/1.1 200 OK\r\nContent-Length: {len(payload)}\r\n\r\n".encode() + payload)

            response = http.client.HTTPResponse(ResponseSocket())
            response.begin()
            connection = MagicMock()
            connection.getresponse.return_value = response
            with patch("gemini_dispatch.http.client.HTTPSConnection", return_value=connection):
                docker("exec", container, "python3", "-I", "-c", """
import json, os, urllib.request
assert os.listdir('/sys/class/net') == ['lo']
assert 'GEMINI_API_KEY' not in os.environ
request = urllib.request.Request('http://127.0.0.1:18080/v1beta/models/gemini-3.8-flash:generateContent?key=local-token',
    data=json.dumps({'contents': [{'parts': [{'text': 'hello'}]}]}).encode())
with urllib.request.urlopen(request, timeout=10) as response:
    body = response.read()
    assert response.status == 200 and b'answer' in body and b'HOST_ONLY_FAKE_KEY' not in body
""")
            assert budget.committed_microdollars() == cost(10, 3)
            docker("rm", "-f", container)
            removed = True
            worker.join(timeout=5)
            assert not worker.is_alive() and not failures, failures
    finally:
        already_failed = sys.exc_info()[0] is not None
        cleanup_errors = []
        try:
            if not removed:
                docker("rm", "-f", container)
        except Exception as error:
            cleanup_errors.append(error)
        if process is not None:
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired as error:
                cleanup_errors.append(error)
                process.kill()
                process.wait(timeout=5)
        if worker is not None:
            worker.join(timeout=5)
            if worker.is_alive():
                cleanup_errors.append(RuntimeError("relay worker did not stop"))
        if process is not None and (worker is None or not worker.is_alive()):
            process.stdin.close()
            process.stdout.close()
        if cleanup_errors and not already_failed:
            raise cleanup_errors[0]
    print("PASS: model request through network-none Docker pipes; budget and key stayed on host")


if __name__ == "__main__":
    check()
