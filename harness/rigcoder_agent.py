"""rigcoder as a Harbor agent.

Harbor (https://github.com/harbor-framework/harbor) runs Terminal-Bench 2.x.
This adapter uploads a prebuilt Linux `rigcoder` binary into the task
container, runs it on the task instruction in /app, and keeps the JSONL
transcript in the trial's agent log directory.

Run it with something like:

    uv tool install harbor
    ./harness/build-linux.sh                      # -> harness/bin/rigcoder-linux-<arch>
    ANTHROPIC_API_KEY=... harbor run \
        -d terminal-bench@2.0 \
        -a harness.rigcoder_agent:RigcoderAgent \
        --ae ANTHROPIC_API_KEY=$ANTHROPIC_API_KEY \
        -m anthropic/claude-opus-5 \
        -i 'hello-*' -n 4 -o harness/runs

from the repository root (so `harness` is importable).
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
        binary = default_binary()
        if not binary.is_file():
            raise FileNotFoundError(
                f"{binary} is missing; run harness/build-linux.sh first "
                "or point RIGCODER_BIN at a Linux build"
            )
        await environment.upload_file(binary, REMOTE_BIN)
        await self.exec_as_root(environment, f"chmod 755 {REMOTE_BIN}")
        # bash is the tool the agent shells through, and the transport needs a
        # CA store (bare ubuntu images ship none): make sure both exist.
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
        for key in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY", "ANTHROPIC_BASE_URL", "OPENAI_BASE_URL"):
            value = self._extra_env.get(key) or os.environ.get(key)
            if value:
                env[key] = value
        timeout = int(os.environ.get("RIGCODER_TIMEOUT_SECS", "1500"))
        max_turns = int(os.environ.get("RIGCODER_MAX_TURNS", "200"))
        command = (
            f"mkdir -p {REMOTE_LOGS} && cd {WORKDIR} && "
            f"{REMOTE_BIN} --cwd {WORKDIR} --max-turns {max_turns} "
            f"--timeout-secs {timeout} --transcript {REMOTE_LOGS}/transcript.jsonl "
            f"{shlex.quote(instruction)} 2>&1 | tee {REMOTE_LOGS}/rigcoder.txt"
        )
        # exec_as_agent raises on a non-zero exit; a failed run is still a
        # trial that should be verified, so exec directly and keep the code.
        result = await environment.exec(
            command=f"set -o pipefail; {command}",
            cwd=WORKDIR,
            env=env,
            timeout_sec=timeout + 60,
        )
        (self.logs_dir / "exit_code.txt").write_text(str(result.return_code))

    def populate_context_post_run(self, context: AgentContext) -> None:
        transcript = self.logs_dir / "transcript.jsonl"
        if not transcript.is_file():
            return
        events = [json.loads(line) for line in transcript.read_text().splitlines() if line.strip()]
        tool_calls = sum(1 for e in events if e.get("kind") == "tool_call")
        failed = [e for e in events if e.get("kind") == "failed"]
        usage = [e for e in events if e.get("kind") == "usage"]
        if usage:
            context.n_input_tokens = sum(e.get("input_tokens", 0) for e in usage)
            context.n_output_tokens = sum(e.get("output_tokens", 0) for e in usage)
            context.n_cache_tokens = sum(e.get("cached_input_tokens", 0) for e in usage)
        context.metadata = {
            "events": len(events),
            "tool_calls": tool_calls,
            "settled": any(e.get("kind") == "settled" for e in events),
            "failure": failed[-1] if failed else None,
        }
