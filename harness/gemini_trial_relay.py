"""Host process supervising one task relay. Parent EOF stops the task container.

Run this in its own process: terminating the process also closes any blocked
upstream call, whose durable reservation remains charged if usage is unknown.
"""
import os
import selectors
import subprocess
import sys
import threading
import time

from artifact_capture import bounded_command
from gemini_budget import Budget
from gemini_relay import REQUEST_LIMIT, read_frame, serve

MODULES = ("gemini_budget", "gemini_usage", "gemini_dispatch", "gemini_gateway", "gemini_relay")


class Admission:
    """Serialize shutdown with reservation admission, not with upstream I/O."""
    def __init__(self, budget):
        self.budget = budget
        self.lock = threading.Lock()
        self.stopped = False

    def reserve(self, *args):
        with self.lock:
            if self.stopped:
                raise ValueError("task relay admission closed")
            return self.budget.reserve(*args)

    def settle(self, *args):
        return self.budget.settle(*args)

    def stop(self):
        with self.lock:
            self.stopped = True


def entry(sources):
    """Dedicated process entry point; failures never print secrets or payloads."""
    try:
        if len(sys.argv) != 6:
            raise ValueError("invalid task relay arguments")
        control(sys.argv[1], Budget(sys.argv[2], context=sys.argv[5]), sys.argv[3], os.environ["GEMINI_API_KEY"],
                sources, int(sys.argv[4]))
    except BaseException:
        sys.stderr.write("task relay failed; retained budget reservations remain charged\n")
        return 1
    return 0


def remote_source(sources, timeout):
    script = "import sys, types, time\n"
    for name in MODULES:
        script += f"m = types.ModuleType({name!r}); sys.modules[{name!r}] = m\nexec({sources[name]!r}, m.__dict__)\n"
    script += f"""
from gemini_relay import endpoint, write_frame, REQUEST_LIMIT
deadline = time.monotonic() + {timeout!r}
gateway = endpoint(0, 1, 18080, 'task-relay', deadline)
write_frame(1, b'ready', REQUEST_LIMIT, deadline)
gateway.serve_forever()
"""
    return script


def control(container, budget, phase, api_key, sources, timeout, parent_fd=0, ready_fd=1):
    """Own relay readiness and shutdown; never place api_key in Docker argv/env."""
    import re
    if (not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", container)
            or phase not in ("development", "holdout") or not api_key
            or type(timeout) is not int or not 1 <= timeout <= 7200):
        raise ValueError("invalid task relay configuration")
    budget.committed_microdollars()  # Existing validated ledger; never initialize/reset.
    deadline = time.monotonic() + timeout
    process = subprocess.Popen(["docker", "exec", "-i", container, "python3", "-I", "-c",
                                remote_source(sources, timeout)], stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    errors = []
    admission = Admission(budget)

    def dispatch():
        try:
            serve(process.stdout.fileno(), process.stdin.fileno(), admission, phase, api_key, deadline)
        except Exception as error:
            errors.append(error)

    failed = False
    try:
        if read_frame(process.stdout.fileno(), REQUEST_LIMIT, min(deadline, time.monotonic() + 30)) != b"ready":
            raise ValueError("task relay did not become ready")
        worker = threading.Thread(target=dispatch, daemon=True)
        worker.start()
        os.write(ready_fd, b"ready\n")
        with selectors.DefaultSelector() as selector:
            selector.register(parent_fd, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("task relay wall limit reached")
                if selector.select(min(remaining, 0.25)):
                    os.read(parent_fd, 1)  # Stop command or parent EOF.
                    break
                if not worker.is_alive():
                    raise ValueError("task relay transport ended before shutdown")
    except BaseException:
        failed = True
        raise
    finally:
        admission.stop()  # No new reservations while Docker cleanup is pending.
        cleanup_error = None
        try:
            bounded_command(["docker", "stop", "--time", "0", container], 4096, 30)
        except Exception as error:
            cleanup_error = error
        # Independently reap the Docker client even when daemon cleanup fails.
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        # The daemon worker may be in HTTPS. The owning host process must exit
        # now; do not reuse its descriptors or wait for an unbounded upstream.
        if not failed and (cleanup_error is not None or errors):
            raise ValueError("task relay shutdown failed") from cleanup_error
