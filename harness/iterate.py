#!/usr/bin/env python3
"""The self-improvement loop: evaluate rigcoder on a benchmark slice, keep
what scores better, and have rigcoder itself propose the next change.

One generation:
  1. build   - cross-build the Linux binary (harness/build-linux.sh)
  2. evaluate- `harbor run` on the chosen tasks; read per-trial rewards
  3. select  - score >= best: commit on the `evolve` branch and advance;
               otherwise revert the mutable files to the last kept commit
  4. improve - write a failure report from the trial transcripts and
               verifier output, then run the host `rigcoder` binary on this
               repository with a meta-task that may only edit the files in
               MUTABLE, and must leave `cargo check` green

Everything is recorded in harness/ledger.jsonl. Run from the repo root:

    python3 harness/iterate.py --generations 5 -i 'hello-*' -i 'fix-*'

Flags mirror `harbor run` where they overlap. Use --dry-run to print the
commands without running them.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUNS = ROOT / "harness" / "runs"
LEDGER = ROOT / "harness" / "ledger.jsonl"

# What the improvement step is allowed to change. Everything else is the
# harness's, not the agent's.
MUTABLE = [
    "crates/rigcoder/src/prompt.md",
    "crates/rigcoder/src/tools.rs",
    "crates/rigcoder/src/lib.rs",
    "crates/rigcoder/src/session.rs",
    "crates/rigcoder-cli/src/main.rs",
]

META_TASK = """You are improving rigcoder, the coding agent in this repository, so it scores higher on Terminal-Bench.

Read {report} first: it has the benchmark results of the current version, and for every failed task the instruction, the tail of the agent's transcript, and the verifier's output.

Then change the agent. You may only edit these files: {mutable}. Typical levers, in order of leverage: the system prompt (prompt.md: process, verification habits, when to stop), tool descriptions and behaviours (tools.rs: output limits, timeouts, error messages the model can act on), the agent's settings (lib.rs: max tokens, tool concurrency), and how tool results are shaped (session.rs).

Rules:
- Make one coherent improvement aimed at the failure patterns you see, not many unrelated tweaks.
- Do not touch the harness/ directory, Cargo.toml files, or the model choice.
- Run `cargo check --workspace` with bash and make it pass before you finish.
- Finish with a short note: what you changed and which failures it targets. Write that note to {note}.
"""


def sh(cmd: list[str] | str, *, dry: bool, cwd: Path = ROOT, env: dict | None = None, check=True) -> subprocess.CompletedProcess:
    text = cmd if isinstance(cmd, str) else " ".join(shlex.quote(c) for c in cmd)
    print(f"$ {text}", flush=True)
    if dry:
        return subprocess.CompletedProcess(cmd, 0, "", "")
    return subprocess.run(cmd, cwd=cwd, env={**os.environ, **(env or {})}, shell=isinstance(cmd, str), check=check, text=True)


def build(args) -> None:
    if args.no_build:
        return
    sh(["bash", "harness/build-linux.sh"], dry=args.dry_run)


def evaluate(args, gen: int) -> tuple[float, dict[str, float], Path]:
    job = f"gen-{gen:03d}-{int(time.time())}"
    cmd = ["harbor", "run", "-d", args.dataset, "-a", "harness.rigcoder_agent:RigcoderAgent",
           "-m", args.model, "-n", str(args.n_concurrent), "-o", str(RUNS), "--job-name", job]
    for pattern in args.include:
        cmd += ["-i", pattern]
    for pattern in args.exclude:
        cmd += ["-x", pattern]
    for key in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY"):
        if os.environ.get(key):
            cmd += ["--ae", f"{key}={os.environ[key]}"]
    sh(cmd, dry=args.dry_run, env={"PYTHONPATH": str(ROOT)}, check=False)
    job_dir = RUNS / job
    if args.dry_run:
        return 0.0, {}, job_dir
    rewards = read_rewards(job_dir)
    score = sum(rewards.values()) / len(rewards) if rewards else 0.0
    return score, rewards, job_dir


def read_rewards(job_dir: Path) -> dict[str, float]:
    """Per-trial reward from every trial's result.json under the job dir."""
    rewards: dict[str, float] = {}
    for result in sorted(job_dir.glob("*/result.json")):
        try:
            data = json.loads(result.read_text())
        except json.JSONDecodeError:
            continue
        name = trial_name(data, result.parent)
        verifier = data.get("verifier_result") or {}
        values = (verifier.get("rewards") or {}) if isinstance(verifier, dict) else {}
        reward = float(values.get("reward", next(iter(values.values()), 0.0))) if values else 0.0
        rewards[name] = reward
    return rewards


def trial_name(data: dict, trial_dir: Path) -> str:
    for key in ("task_name", "trial_name"):
        if isinstance(data.get(key), str):
            return data[key]
    task_id = data.get("task_id")
    if isinstance(task_id, dict) and isinstance(task_id.get("name"), str):
        return task_id["name"]
    return trial_dir.name


def tail(path: Path, chars: int) -> str:
    if not path.is_file():
        return "(missing)"
    text = path.read_text(errors="replace")
    return text if len(text) <= chars else "…" + text[-chars:]


def write_report(gen: int, score: float, rewards: dict[str, float], job_dir: Path, best: float) -> Path:
    lines = [f"# Generation {gen}", "", f"Score: {score:.3f} (best so far {best:.3f}) over {len(rewards)} trial(s)", ""]
    lines.append("| task | reward |")
    lines.append("|---|---|")
    for name, reward in sorted(rewards.items()):
        lines.append(f"| {name} | {reward:.2f} |")
    lines.append("")
    for trial in sorted(job_dir.glob("*/")):
        result = trial / "result.json"
        if not result.is_file():
            continue
        data = json.loads(result.read_text())
        name = trial_name(data, trial)
        if rewards.get(name, 0.0) >= 1.0:
            continue
        lines += [f"## Failed: {name}", ""]
        instruction = next(iter(trial.glob("**/instruction.md")), None)
        if instruction:
            lines += ["### Instruction", "", tail(instruction, 3000), ""]
        lines += ["### Agent transcript (tail)", "", "```", tail(trial / "agent" / "rigcoder.txt", 6000), "```", ""]
        verifier_dir = trial / "verifier"
        if verifier_dir.is_dir():
            for out in sorted(verifier_dir.glob("*")):
                if out.is_file() and out.suffix in (".txt", ".log", ".json", ".xml"):
                    lines += [f"### Verifier: {out.name}", "", "```", tail(out, 3000), "```", ""]
    report = job_dir / "report.md"
    report.write_text("\n".join(lines))
    return report


def git(args_, *, dry: bool, check=True) -> subprocess.CompletedProcess:
    return sh(["git", *args_], dry=dry, check=check)


def improve(args, report: Path, gen: int) -> None:
    note = report.parent / "improvement.md"
    task = META_TASK.format(report=report, mutable=", ".join(MUTABLE), note=note)
    host_bin = os.environ.get("RIGCODER_HOST_BIN") or str(ROOT / "target" / "release" / "rigcoder")
    if not Path(host_bin).is_file() and not args.dry_run:
        sh(["cargo", "build", "--release", "-p", "rigcoder-cli"], dry=args.dry_run)
    cmd = [host_bin, "--cwd", str(ROOT), "--max-turns", "80", "--timeout-secs", "1800",
           "--transcript", str(report.parent / "improve-transcript.jsonl"), task]
    env = {"RIGCODER_PROVIDER": args.meta_provider}
    if args.meta_model:
        env["RIGCODER_MODEL"] = args.meta_model
    sh(cmd, dry=args.dry_run, env=env, check=False)
    # Whatever the meta-run did outside the mutable set is undone.
    if not args.dry_run:
        changed = subprocess.run(["git", "diff", "--name-only"], cwd=ROOT, capture_output=True, text=True).stdout.split()
        outside = [f for f in changed if f not in MUTABLE]
        if outside:
            git(["checkout", "--", *outside], dry=False)
        # Refuse a change that does not compile: revert to the last kept state.
        if subprocess.run(["cargo", "check", "--workspace"], cwd=ROOT).returncode != 0:
            print("improvement does not compile; reverting", flush=True)
            git(["checkout", "--", *MUTABLE], dry=False)


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--generations", type=int, default=3)
    p.add_argument("--dataset", default="terminal-bench@2.0")
    p.add_argument("-i", "--include", action="append", default=[], help="task name glob (repeatable)")
    p.add_argument("-x", "--exclude", action="append", default=[])
    p.add_argument("-m", "--model", default="anthropic/claude-opus-5", help="provider/model the benchmarked agent uses")
    p.add_argument("--meta-provider", default="anthropic")
    p.add_argument("--meta-model", default=None, help="model the improvement step uses (default: provider default)")
    p.add_argument("-n", "--n-concurrent", type=int, default=4)
    p.add_argument("--no-build", action="store_true")
    p.add_argument("--no-improve", action="store_true", help="evaluate only")
    p.add_argument("--dry-run", action="store_true")
    args = p.parse_args()

    RUNS.mkdir(parents=True, exist_ok=True)
    branch = subprocess.run(["git", "rev-parse", "--abbrev-ref", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    if branch != "evolve" and not args.dry_run:
        git(["checkout", "-B", "evolve"], dry=False)
    best = -1.0
    if LEDGER.is_file():
        for line in LEDGER.read_text().splitlines():
            entry = json.loads(line)
            if entry.get("kept"):
                best = max(best, entry["score"])

    for gen in range(args.generations):
        build(args)
        score, rewards, job_dir = evaluate(args, gen)
        kept = score >= best
        head = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
        if kept:
            best = score
            if not args.dry_run and subprocess.run(["git", "status", "--porcelain", *MUTABLE], cwd=ROOT, capture_output=True, text=True).stdout.strip():
                git(["add", *MUTABLE], dry=False)
                git(["commit", "-q", "-m", f"evolve: generation {gen} scored {score:.3f}"], dry=False)
        else:
            print(f"generation {gen} scored {score:.3f} < best {best:.3f}; reverting", flush=True)
            git(["checkout", "--", *MUTABLE], dry=args.dry_run)
        entry = {"generation": gen, "commit": head, "score": score, "best": best, "kept": kept,
                 "rewards": rewards, "job_dir": str(job_dir), "time": time.time()}
        print(json.dumps(entry), flush=True)
        if not args.dry_run:
            with LEDGER.open("a") as f:
                f.write(json.dumps(entry) + "\n")
        if args.no_improve or gen == args.generations - 1:
            continue
        report = write_report(gen, score, rewards, job_dir, best) if not args.dry_run else job_dir / "report.md"
        improve(args, report, gen)
    return 0


if __name__ == "__main__":
    sys.exit(main())
