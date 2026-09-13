"""rigcoder as a Harbor installed agent.

Harbor (https://github.com/harbor-framework/harbor) runs Terminal-Bench 2.x
and, since 0.22, enforces per-phase network policy through an egress sidecar.
This adapter uploads the prebuilt Linux `rigcoder` binary from
`harness/build-linux.sh` into the task container, runs it on the task
instruction in /app, and keeps the transcript, effect log and observation
trace in the trial's agent log directory.

    ./harness/build-linux.sh
    GEMINI_API_KEY=... harbor run -p harness/tasks/cancel-async-tasks \
        -a harness.rigcoder_agent:RigcoderAgent --ae GEMINI_API_KEY=$GEMINI_API_KEY \
        -m gemini/gemini-3.8-flash -o harness/runs

from the repository root, so `harness` is importable. Only the provider key
passed with --ae reaches the container; it is never printed.
"""

from __future__ import annotations

import json
import os
import platform
import shlex
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

HERE = Path(__file__).resolve().parent
REMOTE_BIN = "/usr/local/bin/rigcoder"
REMOTE_LOGS = "/logs/agent"
WORKDIR = "/app"
PROVIDER_KEYS = ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY")
PROVIDER_ENV = ("ANTHROPIC_BASE_URL", "OPENAI_BASE_URL")


def default_binary() -> Path:
    """The Linux binary `build-linux.sh` produced for the container's arch."""
    explicit = os.environ.get("RIGCODER_BIN")
    if explicit:
        return Path(explicit)
    arch = os.environ.get("RIGCODER_LINUX_ARCH") or (
        "aarch64" if platform.machine() in ("arm64", "aarch64") else "x86_64"
    )
    return HERE / "bin" / f"rigcoder-linux-{arch}"


class RigcoderAgent(BaseInstalledAgent):
    """A prebuilt `rigcoder` binary driven on the task instruction."""

    @staticmethod
    def name() -> str:
        return "rigcoder"

    def version(self) -> str | None:
        return os.environ.get("RIGCODER_VERSION", "dev")

    async def install(self, environment: BaseEnvironment) -> None:
        # Harbor may run a task's prebuilt amd64 image under emulation on an
        # arm64 host; the binary must match the container, not the host.
        machine = await self.exec_as_root(environment, "uname -m")
        arch = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}.get(
            (machine.stdout or "").strip()
        )
        binary = (
            Path(os.environ["RIGCODER_BIN"])
            if os.environ.get("RIGCODER_BIN")
            else HERE / "bin" / f"rigcoder-linux-{arch}"
            if arch
            else default_binary()
        )
        if not binary.is_file():
            raise FileNotFoundError(
                f"{binary} is missing for container arch {machine.stdout.strip()!r}; "
                f"run ARCH={arch or 'x86_64'} harness/build-linux.sh first "
                "or point RIGCODER_BIN at a Linux build"
            )
        await environment.upload_file(binary, REMOTE_BIN)
        await self.exec_as_root(environment, f"chmod 755 {REMOTE_BIN}")
        # bash is the tool the agent shells through, and the transport needs a
        # CA store (bare ubuntu images ship none): make sure both exist. This
        # runs in the environment baseline network phase, not the agent phase.
        await self.exec_as_root(
            environment,
            "(command -v bash >/dev/null && [ -s /etc/ssl/certs/ca-certificates.crt ]) "
            "|| (apt-get update -qq && apt-get install -y -qq bash ca-certificates) || true",
        )

    def _provider_and_model(self) -> tuple[str, str | None]:
        name = self.model_name or "gemini/gemini-3.8-flash"
        if "/" in name:
            provider, model = name.split("/", 1)
        else:
            provider, model = "anthropic", name
        return provider, model

    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        provider, model = self._provider_and_model()
        env = {
            "RIGCODER_PROVIDER": provider,
            "RUST_LOG": os.environ.get("RIGCODER_RUST_LOG", "warn"),
        }
        if model:
            env["RIGCODER_MODEL"] = model
        for key in PROVIDER_KEYS + PROVIDER_ENV:
            value = self._extra_env.get(key) or os.environ.get(key)
            if value:
                env[key] = value
        timeout = int(os.environ.get("RIGCODER_TIMEOUT_SECS", "1500"))
        max_turns = int(os.environ.get("RIGCODER_MAX_TURNS", "200"))
        # The instruction goes through a file, never argv: it is untrusted text.
        task_file = self.logs_dir / "instruction.md"
        task_file.write_text(instruction)
        await environment.exec(command=f"mkdir -p {REMOTE_LOGS}", user="root")
        await environment.upload_file(task_file, f"{REMOTE_LOGS}/instruction.md")
        if os.environ.get("RIGCODER_NETWORK_PROBE"):
            # Diagnostic: from inside the agent phase, through the same exec
            # path the bash tool uses, is arbitrary egress denied? Records the
            # outcome; never changes the trial.
            # A bare TCP connect is not evidence: an egress proxy accepts the
            # connection and then drops it. Require an HTTP response body.
            probe = await environment.exec(
                command="python3 - <<'EOF'\n"
                "import urllib.request\n"
                "for url in ('http://93.184.215.14/', 'https://example.com/', 'http://example.com/'):\n"
                "    try:\n"
                "        with urllib.request.urlopen(url, timeout=8) as r:\n"
                "            print(url, 'REACHED', r.status, len(r.read()))\n"
                "    except Exception as e:\n"
                "        print(url, 'DENIED', type(e).__name__, str(e)[:80])\n"
                "EOF",
                user="root",
                timeout_sec=60,
            )
            (self.logs_dir / "network-probe.txt").write_text(
                f"stdout={probe.stdout!r}\nstderr={probe.stderr!r}\nreturn_code={probe.return_code}\n"
            )
        command = (
            f"cd {WORKDIR} && {REMOTE_BIN} --cwd {WORKDIR} --max-turns {max_turns} "
            f"--timeout-secs {timeout} --task-file {REMOTE_LOGS}/instruction.md "
            f"--transcript {REMOTE_LOGS}/transcript.jsonl "
            f"--effect-log {REMOTE_LOGS}/effects.json "
            f"--observations {REMOTE_LOGS}/observations.json "
            f"> {REMOTE_LOGS}/rigcoder.txt 2>&1; status=$?; "
            f"echo $status > {REMOTE_LOGS}/exit_code.txt; exit $status"
        )
        # exec_as_root raises on a non-zero exit; a failed run is still a
        # trial that should be verified, so exec directly and keep the code.
        result = await environment.exec(
            command=command,
            cwd=WORKDIR,
            env=env,
            timeout_sec=timeout + 60,
            user="root",
        )
        (self.logs_dir / "exit_code.txt").write_text(str(result.return_code))

    def populate_context_post_run(self, context: AgentContext) -> None:
        transcript = self.logs_dir / "transcript.jsonl"
        if not transcript.is_file():
            return
        events = []
        for line in transcript.read_text().splitlines():
            if line.strip():
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    context.metadata = {"transcript": "malformed"}
                    return
        usage = [e for e in events if e.get("kind") == "usage"]
        if usage:
            # Same sums rigcoder-bench records; missing fields are unknown, not zero.
            def total(field: str) -> int | None:
                values = [e.get(field) for e in usage]
                return sum(values) if all(isinstance(v, int) for v in values) else None

            context.n_input_tokens = total("input_tokens")
            context.n_output_tokens = total("output_tokens")
            context.n_cache_tokens = total("cached_input_tokens")
        failed = [e for e in events if e.get("kind") == "failed"]
        context.metadata = {
            "events": len(events),
            "tool_calls": sum(1 for e in events if e.get("kind") == "tool_call"),
            "settled": any(e.get("kind") == "settled" for e in events),
            "failure": failed[-1] if failed else None,
        }
