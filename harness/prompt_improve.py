"""Trusted prompt-only launcher; repository apply/acceptance stays with the caller."""
import hashlib
import json
from pathlib import Path
import shutil
import sys

from artifact_capture import bounded_command
from improver_sandbox import command
from prompt_workspace import PROMPT, PromptWorkspace

LIMITS = '''import os, resource, sys
resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
resource.setrlimit(resource.RLIMIT_CPU, (120, 120))
resource.setrlimit(resource.RLIMIT_FSIZE, (8388608, 8388608))
resource.setrlimit(resource.RLIMIT_NOFILE, (128, 128))
os.execv(sys.argv[1], sys.argv[1:])
'''


def digest(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def launch(binary, prompt, report, gateway_port, gateway_token, evidence, timeout=1800):
    if type(timeout) is not int or not 1 <= timeout <= 1800:
        raise ValueError("invalid improver wall limit")
    if not isinstance(gateway_token, str) or not gateway_token:
        raise ValueError("gateway token is required")
    evidence = Path(evidence)
    evidence.mkdir(mode=0o700)
    workspace = PromptWorkspace.create(evidence / "workspace", prompt, report)
    executable = evidence / "rigcoder"
    shutil.copyfile(Path(binary).resolve(strict=True), executable)
    executable.chmod(0o500)
    argv = command(executable, workspace.path, gateway_port)
    manifest = {"lane": "prompt", "model": "gemini-3.8-flash", "max_turns": 20,
                "wall_limit_seconds": timeout, "cpu_limit_seconds": 120,
                "file_size_limit_bytes": 8_388_608,
                "baseline_prompt_sha256": workspace.baseline_sha256,
                "development_report_sha256": workspace.report_sha256,
                "binary_sha256": digest(executable), "status": "running"}
    manifest_path = evidence / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, sort_keys=True))
    task = ("Read development.md and crates/rigcoder/src/prompt.md. Propose one narrow prompt "
            "change supported by the development evidence. Edit only that prompt. Write the "
            "hypothesis and rationale to improvement.md. Do not claim an improvement has been "
            "proven; the trusted evaluator will compare the candidate with its baseline.")
    argv += ["--provider", "gemini", "--model", "gemini-3.8-flash",
             "--gemini-gateway", f"http://127.0.0.1:{gateway_port}",
             "--cwd", str(workspace.path), "--max-turns", "20", "--timeout-secs", str(timeout),
             "--allow", str(workspace.path / PROMPT),
             "--allow", str(workspace.path / "improvement.md"),
             "--transcript", str(workspace.path / "transcript.jsonl"),
             "--effect-log", str(workspace.path / "effects.json"),
             "--observations", str(workspace.path / "observations.json"), task]
    environment = {"PATH": "/usr/bin:/bin", "HOME": str(workspace.path),
                   "TMPDIR": str(workspace.path), "RIGCODER_GATEWAY_TOKEN": gateway_token}
    try:
        output = bounded_command([sys.executable, "-I", "-c", LIMITS, *argv], 4_194_304,
                                 timeout, cwd=workspace.path, env=environment)
        (evidence / "console.txt").write_bytes(output)
        proposal = workspace.collect()  # Child and its process group have terminated.
        manifest["candidate_prompt_sha256"] = hashlib.sha256(proposal["prompt"]).hexdigest()
        manifest["status"] = "proposal_collected"
        manifest_path.write_text(json.dumps(manifest, sort_keys=True))
        return proposal
    except BaseException:
        manifest["status"] = "failed"
        manifest_path.write_text(json.dumps(manifest, sort_keys=True))
        raise
