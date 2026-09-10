"""Bounded Docker archive capture for output-only scoring (runner wiring pending)."""
import os
import http.client
import io
from urllib.parse import quote
from pathlib import PurePosixPath
import re
import selectors
import signal
import subprocess
import time

from artifact_score import MAX_ARCHIVE


def bounded_command(arguments, stdout_limit, timeout, request=b""):
    """Capture a trusted CLI under a wall deadline and two bounded pipes."""
    process = subprocess.Popen(arguments, stdin=subprocess.PIPE if request else subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=True, bufsize=0)
    output = bytearray()
    errors = bytearray()
    deadline = time.monotonic() + timeout
    try:
        with selectors.DefaultSelector() as selector:
            pending = memoryview(request)
            if request:
                os.set_blocking(process.stdin.fileno(), False)
                selector.register(process.stdin, selectors.EVENT_WRITE, None)
            selector.register(process.stdout, selectors.EVENT_READ, (output, stdout_limit))
            selector.register(process.stderr, selectors.EVENT_READ, (errors, 65_536))
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ValueError("artifact capture command timed out")
                for key, _ in selector.select(remaining):
                    if key.data is None:
                        written = os.write(key.fd, pending)
                        pending = pending[written:]
                        if not pending:
                            selector.unregister(key.fileobj)
                            process.stdin.close()
                        continue
                    destination, limit = key.data
                    chunk = os.read(key.fd, min(65_536, limit - len(destination) + 1))
                    if not chunk:
                        selector.unregister(key.fileobj)
                    else:
                        destination.extend(chunk)
                        if len(destination) > limit:
                            raise ValueError("artifact capture output exceeded limit")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ValueError("artifact capture command timed out")
            try:
                code = process.wait(timeout=remaining)
            except subprocess.TimeoutExpired as error:
                raise ValueError("artifact capture command timed out") from error
            if code != 0:
                # Candidate-controlled names or daemon diagnostics are not scores.
                raise ValueError("artifact capture command failed")
            return bytes(output)
    finally:
        # Also stop descendants retaining pipes after their parent exits.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        if process.stdin is not None:
            process.stdin.close()
        process.stdout.close()
        process.stderr.close()


def capture(container, artifact, timeout=30):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", container):
        raise ValueError("invalid container identity")
    path = PurePosixPath(artifact)
    if (not artifact.startswith("/") or str(path) != artifact or path.name in ("", ".", "..")
            or ".." in path.parts or "\x00" in artifact or len(artifact) > 4096):
        raise ValueError("invalid artifact path")
    deadline = time.monotonic() + timeout

    def run(arguments, limit, request=b""):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ValueError("artifact capture timed out")
        return bounded_command(arguments, limit, remaining, request)

    run(["docker", "stop", "--time", "0", container], 4096)
    state = run(["docker", "inspect", "--format", "{{.State.Running}}", container], 4096)
    if state.strip() != b"false":
        raise ValueError("candidate container is still running")
    # The CLI transport honors Docker contexts/TLS without exposing their keys.
    # Engine API 1.45 defines 404 as missing container or path. Reinspect the
    # known container after a 404 to distinguish ordinary absence from loss of
    # the container. Never infer absence from CLI stderr wording.
    target = f"/v1.45/containers/{container}/archive?path={quote(artifact, safe='')}"
    request = f"GET {target} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n".encode("ascii")
    raw = run(["docker", "system", "dial-stdio"], MAX_ARCHIVE + 65_536, request)

    class ResponseSocket:
        def makefile(self, *_args):
            return io.BytesIO(raw)

    try:
        with http.client.HTTPResponse(ResponseSocket()) as response:
            response.begin()
            status = response.status
            # Raw transport bytes are already bounded. Reading to completion
            # is necessary for HTTPResponse to verify Content-Length framing.
            body = response.read()
    except (http.client.HTTPException, OSError) as error:
        raise ValueError("invalid archive response") from error
    if len(body) > MAX_ARCHIVE:
        raise ValueError("artifact archive exceeded limit")
    if status not in (200, 404):
        raise ValueError("artifact archive request failed")
    state = run(["docker", "inspect", "--format", "{{.State.Running}}", container], 4096)
    if state.strip() != b"false":
        raise ValueError("candidate container changed during capture")
    return None if status == 404 else body
