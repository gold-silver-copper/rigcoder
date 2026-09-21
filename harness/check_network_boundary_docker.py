"""Task-network boundary probes on the real network-none relay container.

Extends check_gemini_relay_docker: the same pipe relay with a mocked upstream,
plus candidate-side shell and Python probes that must be denied (direct IP,
hostname, in-container redirect to an external target), loopback services that
must keep working, credential absence, and failure/shutdown accounting. Fake
key, synthetic endpoints, zero provider charges. Diagnostic only: this proves
the transport boundary, not scorer integrity inside a candidate-modified
container.
"""
import http.client
import http.server
import io
import json
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from unittest.mock import MagicMock, patch

from artifact_capture import bounded_command
from gemini_budget import MAX_INPUT, MAX_OUTPUT, Budget, cost
from gemini_relay import REQUEST_LIMIT, read_frame, serve

IMAGE = "python:3.13-slim"
# Host-side synthetic "external" endpoint: reachable from the host, and only
# the host, so a denied container probe is a real denial rather than a dead
# address. TEST-NET-3 and a reserved name cover the no-server cases.
DEAD_IP = "203.0.113.7"
DEAD_NAME = "answers.invalid"

PROBES = r'''
import json, os, socket, subprocess, sys, threading, urllib.request, urllib.error
from http.server import BaseHTTPRequestHandler, HTTPServer

host_port = int(sys.argv[1]); host_ip = sys.argv[2]
results = {}

def record(name, fn):
    try:
        value = fn()
        results[name] = {"denied": False, "detail": str(value)[:120]}
    except Exception as error:
        results[name] = {"denied": True, "detail": type(error).__name__ + ": " + str(error)[:120]}

# Loopback services a task may legitimately run: an in-container HTTP server,
# including one that redirects to an external target (the redirect must fail).
class Local(BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def do_GET(self):
        if self.path == "/redirect-ip":
            self.send_response(302); self.send_header("Location", f"http://{host_ip}:{host_port}/answer"); self.end_headers()
        elif self.path == "/redirect-name":
            self.send_response(302); self.send_header("Location", "http://%s/answer" % sys.argv[3]); self.end_headers()
        elif self.path == "/redirect-gateway":
            self.send_response(302); self.send_header("Location", "http://127.0.0.1:18080/"); self.end_headers()
        else:
            body = b"local-ok"; self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
local = HTTPServer(("127.0.0.1", 18081), Local)
threading.Thread(target=local.serve_forever, daemon=True).start()

record("interfaces", lambda: os.listdir("/sys/class/net"))
# The python image itself exports GPG_KEY; only provider material matters.
record("env_key", lambda: sorted(k for k, v in os.environ.items() if k.startswith("GEMINI") or "HOST_ONLY_FAKE_KEY" in v))
record("proc1_env", lambda: b"HOST_ONLY_FAKE_KEY" in open("/proc/1/environ", "rb").read())
record("python_direct_ip", lambda: socket.create_connection((host_ip, host_port), timeout=3).close())
record("python_dead_ip", lambda: socket.create_connection((sys.argv[4], 80), timeout=3).close())
record("python_hostname", lambda: urllib.request.urlopen("http://%s/" % sys.argv[3], timeout=3).read())
record("python_dns", lambda: socket.gethostbyname(sys.argv[3]))
record("python_redirect_ip", lambda: urllib.request.urlopen("http://127.0.0.1:18081/redirect-ip", timeout=3).read())
record("python_redirect_name", lambda: urllib.request.urlopen("http://127.0.0.1:18081/redirect-name", timeout=3).read())
record("shell_devtcp_ip", lambda: subprocess.run(["bash", "-c", f"exec 3<>/dev/tcp/{host_ip}/{host_port}"], capture_output=True, timeout=5, check=True))
record("shell_devtcp_name", lambda: subprocess.run(["bash", "-c", "exec 3<>/dev/tcp/%s/80" % sys.argv[3]], capture_output=True, timeout=5, check=True))
record("shell_getent", lambda: subprocess.run(["getent", "hosts", sys.argv[3]], capture_output=True, timeout=5, check=True))
record("shell_pip", lambda: subprocess.run([sys.executable, "-m", "pip", "download", "--no-deps", "-d", "/tmp/pipdl", "--timeout", "3", "--retries", "0", "ptyprocess"], capture_output=True, timeout=30, check=True))
record("loopback_http", lambda: urllib.request.urlopen("http://127.0.0.1:18081/ok", timeout=3).read())
def redirect_gateway():
    # The relay gateway answers GET with 501: reaching it through a loopback redirect is the point.
    try:
        urllib.request.urlopen("http://127.0.0.1:18081/redirect-gateway", timeout=3)
    except urllib.error.HTTPError as error:
        if error.code == 501:
            return "gateway reached via loopback redirect (501)"
        raise
    raise AssertionError("gateway did not answer")
record("loopback_redirect_gateway", redirect_gateway)
record("loopback_tcp_echo", lambda: (lambda s: (s.bind(("127.0.0.1", 0)), s.listen(1), socket.create_connection(s.getsockname(), timeout=3).close(), s.close()))(socket.socket()))

def model():
    request = urllib.request.Request("http://127.0.0.1:18080/v1beta/models/gemini-3.8-flash:generateContent?key=local-token",
        data=json.dumps({"contents": [{"parts": [{"text": "hello"}]}]}).encode())
    with urllib.request.urlopen(request, timeout=10) as response:
        body = response.read()
        assert response.status == 200 and b"answer" in body and b"HOST_ONLY_FAKE_KEY" not in body
        return "200 mocked completion, key absent from body"
record("model_via_relay", model)

def model_upstream_failure():
    request = urllib.request.Request("http://127.0.0.1:18080/v1beta/models/gemini-3.8-flash:generateContent?key=local-token",
        data=json.dumps({"contents": [{"parts": [{"text": "FAIL"}]}]}).encode())
    try:
        urllib.request.urlopen(request, timeout=10)
    except urllib.error.HTTPError as error:
        return "status %d" % error.code
    raise AssertionError("upstream failure was not reported")
record("model_upstream_failure", model_upstream_failure)
print(json.dumps(results))
'''


class Synthetic(http.server.BaseHTTPRequestHandler):
    hits = []

    def log_message(self, *_args):
        pass

    def do_GET(self):
        Synthetic.hits.append(self.path)
        body = b"SYNTHETIC ANSWER MATERIAL"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def host_ip():
    # Any non-loopback host address: the container has no route to it either way.
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.connect((DEAD_IP, 9))
        return probe.getsockname()[0]
    except OSError:
        return "127.0.0.1"
    finally:
        probe.close()


def check():
    def docker(*args, timeout=30):
        return bounded_command(["docker", *args], 65_536, timeout)

    docker("pull", IMAGE, timeout=120)
    image = docker("image", "inspect", "--format", "{{.Id}}", IMAGE).decode().strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
        raise ValueError("invalid relay image identity")
    container = "rigcoder-boundary-check-" + uuid.uuid4().hex
    script = "import sys, types, time\n"
    for name in ("gemini_budget", "gemini_usage", "gemini_dispatch", "gemini_gateway", "gemini_relay"):
        source = Path(__file__).with_name(name + ".py").read_text()
        script += f"m = types.ModuleType({name!r}); sys.modules[{name!r}] = m\nexec({source!r}, m.__dict__)\n"
    script += """
from gemini_relay import endpoint, write_frame, REQUEST_LIMIT
deadline = time.monotonic() + 120
gateway = endpoint(0, 1, 18080, 'local-token', deadline)
write_frame(1, b'ready', REQUEST_LIMIT, deadline)
gateway.serve_forever()
"""
    synthetic = http.server.HTTPServer(("0.0.0.0", 0), Synthetic)
    synthetic_port = synthetic.server_address[1]
    threading.Thread(target=synthetic.serve_forever, daemon=True).start()
    address = host_ip()
    # Control: the host itself reaches the synthetic endpoint.
    with socket.create_connection((address, synthetic_port), timeout=3):
        pass

    process = None
    worker = None
    failures = []
    removed = False
    report = {"image": image, "container": container, "synthetic_endpoint": f"{address}:{synthetic_port}"}
    try:
        docker("run", "-d", "--name", container, "--network", "none", "--read-only",
               "--tmpfs", "/tmp", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
               "--memory", "256m", "--cpus", "1", "--pids-limit", "64",
               image, "python3", "-c", "import time; time.sleep(3600)")
        inspect = json.loads(docker("inspect", container).decode())[0]
        report["network_mode"] = inspect["HostConfig"]["NetworkMode"]
        report["networks"] = sorted(inspect["NetworkSettings"]["Networks"])
        # Docker 29 dropped the legacy top-level IPAddress; read the per-network one.
        report["ip_address"] = [n.get("IPAddress", "") for n in inspect["NetworkSettings"]["Networks"].values()]
        assert report["network_mode"] == "none" and report["networks"] == ["none"] and report["ip_address"] == [""]
        with tempfile.TemporaryDirectory() as directory:
            budget = Budget(Path(directory) / "budget.sqlite")
            budget.initialize()
            process = subprocess.Popen(["docker", "exec", "-i", container, "python3", "-I", "-c", script],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            deadline = time.monotonic() + 120
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

            def connection_for(*_args, **_kwargs):
                connection = MagicMock()

                def request(_method, _path, body=b"", headers=None):
                    # The mocked upstream fails one request; the candidate must
                    # see an error status and the ledger must not charge it.
                    connection.failing = b"FAIL" in body

                def getresponse():
                    if connection.failing:
                        raise ConnectionResetError("synthetic upstream failure")

                    class ResponseSocket:
                        def makefile(self, *_a):
                            return io.BytesIO(f"HTTP/1.1 200 OK\r\nContent-Length: {len(payload)}\r\n\r\n".encode() + payload)
                    response = http.client.HTTPResponse(ResponseSocket())
                    response.begin()
                    return response
                connection.request.side_effect = request
                connection.getresponse.side_effect = getresponse
                return connection

            with patch("gemini_dispatch.http.client.HTTPSConnection", side_effect=connection_for):
                output = docker("exec", container, "python3", "-I", "-c", PROBES,
                                str(synthetic_port), address, DEAD_NAME, DEAD_IP, timeout=120)
            results = json.loads(output.decode().strip().splitlines()[-1])
            report["probes"] = results
            must_deny = ["python_direct_ip", "python_dead_ip", "python_hostname", "python_dns",
                         "python_redirect_ip", "python_redirect_name", "shell_devtcp_ip",
                         "shell_devtcp_name", "shell_getent", "shell_pip"]
            must_allow = ["loopback_http", "loopback_redirect_gateway", "loopback_tcp_echo", "model_via_relay",
                          "model_upstream_failure"]
            for name in must_deny:
                assert results[name]["denied"], (name, results[name])
            for name in must_allow:
                assert not results[name]["denied"], (name, results[name])
            assert results["interfaces"]["detail"] == "['lo']"
            assert results["env_key"]["detail"] == "[]", results["env_key"]
            assert results["proc1_env"]["detail"] == "False"
            assert "501" in results["loopback_redirect_gateway"]["detail"]
            assert results["model_upstream_failure"]["detail"] == "status 502", results["model_upstream_failure"]
            assert not Synthetic.hits, Synthetic.hits
            # Accounting: the successful completion settles to measured usage; the
            # failed one keeps its full reservation charged (unknown usage is never
            # treated as zero), exactly as gemini_dispatch.send documents.
            expected = cost(10, 3) + cost(MAX_INPUT, MAX_OUTPUT)
            assert budget.committed_microdollars() == expected, (budget.committed_microdollars(), expected)
            report["committed_microdollars"] = {"settled_success": cost(10, 3),
                                                "retained_failed_reservation": cost(MAX_INPUT, MAX_OUTPUT)}
            # Shutdown: removing the container ends the pipe; the host worker
            # exits cleanly and the ledger is unchanged afterwards.
            docker("rm", "-f", container)
            removed = True
            worker.join(timeout=5)
            assert not worker.is_alive() and not failures, failures
            assert budget.committed_microdollars() == expected
    finally:
        synthetic.shutdown()
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
    print(json.dumps(report, indent=1, sort_keys=True))
    print("PASS: network-none candidate denied direct-IP/name/redirect/pip egress; loopback, mocked relay, "
          "failure accounting and shutdown intact; key stayed on host")


if __name__ == "__main__":
    check()
