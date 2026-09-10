"""Real bench/controller/relay/scorer exercise; synthetic agent, zero API calls."""
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import uuid

from gemini_budget import Budget


def agent_source(answer):
    return "#!/usr/bin/env python3\n" + textwrap.dedent(r'''
    import os, pathlib, urllib.error, urllib.request
    assert sorted(os.listdir('/sys/class/net')) == ['lo']
    assert 'GEMINI_API_KEY' not in os.environ
    assert os.environ['RIGCODER_GEMINI_GATEWAY'] == 'http://127.0.0.1:18080'
    assert not os.listdir('/tests'), 'trusted tests were exposed'
    # Probe the ready HTTP listener without any generation request or reservation.
    try:
        urllib.request.urlopen('http://127.0.0.1:18080/', timeout=5)
    except urllib.error.HTTPError as error:
        assert error.code == 501
    else:
        raise AssertionError('unexpected relay probe response')
    pathlib.Path('/logs/verifier/reward.txt').write_text('1')
    ''') + f"pathlib.Path('/app/answer.txt').write_text({answer!r})\n"


def check(binary):
    binary = Path(binary).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="rigcoder-budgeted-trial-") as directory:
        root = Path(directory)
        suffix = uuid.uuid4().hex[:12]
        names = {"dev": "dev-" + suffix, "holdout": "holdout-" + suffix}
        (root / "harness/slices").mkdir(parents=True)
        (root / "harness/ledger.jsonl").write_text("")
        (root / ".gitignore").write_text("harness/runs/\nbudget.sqlite*\n")
        for phase, name in names.items():
            (root / f"harness/slices/{phase}.txt").write_text(name + "\n")
            task = root / "harness/tasks" / name
            (task / "environment").mkdir(parents=True)
            (task / "tests").mkdir()
            (task / "task.toml").write_text('[agent]\ntimeout_sec=30\n[verifier]\ntimeout_sec=30\n')
            (task / "instruction.md").write_text("Write the synthetic answer to /app/answer.txt.")
            (task / "environment/Dockerfile").write_text("FROM python:3.13-slim\nWORKDIR /app\n")
            (task / "tests/test.sh").write_text("exit 99\n")
            (task / "tests/output-line.json").write_text(json.dumps({"artifact": "/app/answer.txt", "expected": "SYNTHETIC"}))
        agent = root / "synthetic-agent"
        agent.write_text("synthetic binary placeholder")
        subprocess.run(["git", "init", "-q", root], check=True)
        subprocess.run(["git", "-C", root, "add", "."], check=True)
        subprocess.run(["git", "-C", root, "-c", "user.name=Budgeted trial canary", "-c",
                        "user.email=canary@example.invalid", "commit", "-qm", "synthetic baseline"], check=True)
        budget = Budget(root / "budget.sqlite")
        budget.initialize()
        try:
            for answer, expected in [("SYNTHETIC", 1.0), ("wrong", 0.0)]:
                agent.write_text(agent_source(answer))
                subprocess.run([binary, "--root", root, "iterate", "--no-improve", "--no-build",
                                "--binary", agent, "--gemini-budget", budget.path, "--generations", "1",
                                "-k", "1", "-n", "1"], check=True, timeout=180,
                               env={**os.environ, "GEMINI_API_KEY": "SYNTHETIC_INVALID_KEY"})
                entries = [json.loads(line) for line in (root / "harness/ledger.jsonl").read_text().splitlines()]
                for entry, phase in zip(entries[-2:], ("development", "holdout"), strict=True):
                    assert entry["score"] == expected, entry
                    job = Path(entry["job_dir"])
                    manifest = json.loads((job / "manifest.json").read_text())
                    assert manifest["budget_phase"] == phase
                    assert manifest["provider_transport"] == "host_budgeted_pipe_relay"
                    assert manifest["binary"]["source_binding"] == "unverified"
                    results = list(job.glob("*/verifier/output-line-result.json"))
                    assert len(results) == 1
                    assert json.loads(results[0].read_text())["reward"] == expected
                    trial = results[0].parent.parent
                    link = json.loads((trial / "budget-link.json").read_text())
                    assert link == {"ledger": str(budget.path), "context": str(trial), "phase": phase}
                assert budget.committed_microdollars() == 0
        finally:
            already_failed = sys.exc_info()[0] is not None
            try:
                listing = subprocess.run(["docker", "ps", "-a", "--format", "{{.ID}} {{.Names}}"],
                                         check=True, capture_output=True, text=True, timeout=15)
                pattern = re.compile(r"rigcoder-(?:dev|holdout)-" + re.escape(suffix) + r"-1-[0-9]+")
                owned = []
                for line in listing.stdout.splitlines():
                    identity, name = line.split()
                    if pattern.fullmatch(name) and re.fullmatch(r"[0-9a-f]{12,64}", identity):
                        owned.append(identity)
                if owned:
                    subprocess.run(["docker", "rm", "-f", *owned], check=True, timeout=30)
            except Exception:
                if not already_failed:
                    raise
                print("Budgeted trial check cleanup failed; inspect its uniquely named containers", file=sys.stderr)
    print("PASS: real budgeted bench, relay readiness/shutdown and host scoring; synthetic agent, zero API calls")


if __name__ == "__main__":
    check(sys.argv[1])
