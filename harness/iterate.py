#!/usr/bin/env python3
"""The self-improvement loop: evaluate rigcoder on a benchmark slice, keep
what scores better beyond noise, and have rigcoder itself propose the next
change.

One generation:
  1. build    - cross-build the Linux binary (harness/build-linux.sh)
  2. evaluate - `harbor run` on the dev slice, k attempts per task; read
                every trial's reward and cost
  3. select   - keep when the new score's 95% Wilson lower bound is at least
                the best kept lower bound and the point estimate has not
                dropped (a tie keeps, and is recorded as one); otherwise
                revert the mutable files to the last kept commit
  4. improve  - write a failure report from the trial transcripts and
                verifier output, then run the host `rigcoder` binary on this
                repository with a meta-task that may only edit the files in
                MUTABLE, and must leave `cargo check` green

At the end of a run (or with --holdout) the best kept generation is scored
on the holdout slice, which the meta agent never sees. Everything is
recorded in harness/ledger.jsonl, one JSON object per line. Run from the
repo root:

    python3 harness/iterate.py --generations 3 -k 3

Task selection: --slice dev (default) or holdout, or explicit -i globs.
Use --dry-run to print the commands without running them.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shlex
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUNS = ROOT / "harness" / "runs"
LEDGER = ROOT / "harness" / "ledger.jsonl"
SLICES = ROOT / "harness" / "slices"

# What the improvement step is allowed to change. Everything else is the
# harness's, not the agent's.
MUTABLE = [
    "crates/rigcoder/src/prompt.md",
    "crates/rigcoder/src/tools.rs",
    "crates/rigcoder/src/lib.rs",
    "crates/rigcoder/src/session.rs",
    "crates/rigcoder-cli/src/main.rs",
]

# What the meta agent must never read: the held-out tasks, its own scores,
# and the harness that scores it.
META_FORBIDDEN = [
    "harness/slices/holdout.txt",
    "harness/ledger.jsonl",
    "harness/iterate.py",
]

PROVIDER_KEYS = {"anthropic": "ANTHROPIC_API_KEY", "openai": "OPENAI_API_KEY", "gemini": "GEMINI_API_KEY"}

META_TASK = """You are improving rigcoder, the coding agent in this repository, so it scores higher on Terminal-Bench.

Read {report} first: it has the benchmark results of the current version, and for every failed task the instruction, the tail of the agent's transcript, and the verifier's output.

Then change the agent. You may only edit these files: {mutable}. Typical levers, in order of leverage: the system prompt (prompt.md: process, verification habits, when to stop), tool descriptions and behaviours (tools.rs: output limits, timeouts, error messages the model can act on), the agent's settings (lib.rs: max tokens, tool concurrency), and how tool results are shaped (session.rs).

Rules:
- Make one coherent improvement aimed at the failure patterns you see, not many unrelated tweaks.
- Do not touch the harness/ directory, Cargo.toml files, or the model choice. Do not read {forbidden}.
- Run `cargo check --workspace` with bash and make it pass before you finish.
- Finish with a short note: what you changed and which failures it targets. Write that note to {note}.
"""


# ---------------------------------------------------------------- statistics


def wilson(passed: int, n: int, z: float = 1.96) -> tuple[float, float]:
    """95% Wilson score interval for a pass rate of `passed` in `n` trials."""
    if n == 0:
        return 0.0, 0.0
    p = passed / n
    denom = 1 + z * z / n
    centre = (p + z * z / (2 * n)) / denom
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denom
    return max(0.0, centre - half), min(1.0, centre + half)


def keep_decision(score: float, ci_low: float, best_score: float, best_low: float) -> str:
    """'kept', 'tie' or 'reverted'. A tie is kept: an edit that costs nothing
    should not be punished, and the ledger says it was a tie."""
    if ci_low < best_low or score < best_score:
        return "reverted"
    if ci_low == best_low and score == best_score:
        return "tie"
    return "kept"


def summarize(trials: list[dict]) -> dict:
    """Score, pass@1, pass@k and the interval from a list of trial records."""
    by_task: dict[str, list[float]] = {}
    for t in trials:
        by_task.setdefault(t["task"], []).append(t["reward"])
    n = len(trials)
    passed = sum(1 for t in trials if t["reward"] >= 1.0)
    score = sum(t["reward"] for t in trials) / n if n else 0.0
    pass1 = (sum(sum(1 for r in rs if r >= 1.0) / len(rs) for rs in by_task.values()) / len(by_task)) if by_task else 0.0
    passk = (sum(1 for rs in by_task.values() if any(r >= 1.0 for r in rs)) / len(by_task)) if by_task else 0.0
    low, high = wilson(passed, n)
    return {
        "score": score,
        "pass1": pass1,
        "passk": passk,
        "ci_low": low,
        "ci_high": high,
        "trials": n,
        "tasks": len(by_task),
        "rewards": {task: rs for task, rs in sorted(by_task.items())},
        "cost": {
            "input_tokens": sum(t["input_tokens"] or 0 for t in trials),
            "output_tokens": sum(t["output_tokens"] or 0 for t in trials),
            "tool_calls": sum(t["tool_calls"] or 0 for t in trials),
            "wall_seconds": sum(t["wall_seconds"] or 0 for t in trials),
        },
    }


# ---------------------------------------------------------------- shell


def redact(arg: str) -> str:
    """`KEY=value` arguments that look like secrets print as `KEY=***`."""
    key, sep, value = arg.partition("=")
    if sep and key.isupper() and any(word in key for word in ("KEY", "TOKEN", "SECRET")):
        return f"{key}=***"
    return arg


def sh(cmd: list[str] | str, *, dry: bool, cwd: Path = ROOT, env: dict | None = None, check=True) -> subprocess.CompletedProcess:
    text = cmd if isinstance(cmd, str) else " ".join(shlex.quote(redact(c)) for c in cmd)
    print(f"$ {text}", flush=True)
    if dry:
        return subprocess.CompletedProcess(cmd, 0, "", "")
    return subprocess.run(cmd, cwd=cwd, env={**os.environ, **(env or {})}, shell=isinstance(cmd, str), check=check, text=True)


def git(args_, *, dry: bool, check=True) -> subprocess.CompletedProcess:
    return sh(["git", *args_], dry=dry, check=check)


def git_out(*args_) -> str:
    return subprocess.run(["git", *args_], cwd=ROOT, capture_output=True, text=True).stdout.strip()


# ---------------------------------------------------------------- slices


def read_slice(name: str) -> list[str]:
    path = SLICES / f"{name}.txt"
    tasks = []
    for line in path.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            tasks.append(line)
    if not tasks:
        raise SystemExit(f"{path} names no tasks")
    return tasks


# ---------------------------------------------------------------- build, evaluate


def build(args) -> None:
    if args.no_build:
        return
    sh(["bash", "harness/build-linux.sh"], dry=args.dry_run)


def evaluate(args, label: str, tasks: list[str], attempts: int) -> tuple[list[dict], Path]:
    job = f"{label}-{int(time.time())}"
    cmd = ["harbor", "run", "-d", args.dataset, "-a", "harness.rigcoder_agent:RigcoderAgent",
           "-m", args.model, "-n", str(args.n_concurrent), "-k", str(attempts), "-o", str(RUNS), "--job-name", job]
    for task in tasks:
        cmd += ["-i", task]
    for pattern in args.exclude:
        cmd += ["-x", pattern]
    if args.force_build:
        cmd.append("--force-build")
    # Only the key the benchmarked provider needs crosses into the container.
    key = PROVIDER_KEYS.get(args.model.split("/", 1)[0])
    if key and os.environ.get(key):
        cmd += ["--ae", f"{key}={os.environ[key]}"]
    sh(cmd, dry=args.dry_run, env={"PYTHONPATH": str(ROOT)}, check=False)
    job_dir = RUNS / job
    if args.dry_run:
        return [], job_dir
    return read_trials(job_dir), job_dir


def read_trials(job_dir: Path) -> list[dict]:
    """One record per trial: task, reward, and what it cost."""
    trials = []
    for result in sorted(job_dir.glob("*/result.json")):
        try:
            data = json.loads(result.read_text())
        except json.JSONDecodeError:
            continue
        trial_dir = result.parent
        verifier = data.get("verifier_result") or {}
        values = (verifier.get("rewards") or {}) if isinstance(verifier, dict) else {}
        reward = float(values.get("reward", next(iter(values.values()), 0.0))) if values else 0.0
        agent = data.get("agent_result") or {}
        meta = agent.get("metadata") or {}
        trials.append({
            "task": trial_name(data, trial_dir),
            "trial": trial_dir.name,
            "reward": reward,
            "input_tokens": agent.get("n_input_tokens"),
            "output_tokens": agent.get("n_output_tokens"),
            "cache_tokens": agent.get("n_cache_tokens"),
            "cost_usd": agent.get("cost_usd"),
            "tool_calls": meta.get("tool_calls"),
            "settled": meta.get("settled"),
            "wall_seconds": wall_seconds(data),
            "exception": (data.get("exception_info") or {}).get("exception_type"),
        })
    return trials


def wall_seconds(data: dict) -> float | None:
    from datetime import datetime
    execution = data.get("agent_execution") or {}
    start, end = execution.get("started_at"), execution.get("finished_at")
    if not (start and end):
        return None
    try:
        return (datetime.fromisoformat(end.replace("Z", "+00:00")) - datetime.fromisoformat(start.replace("Z", "+00:00"))).total_seconds()
    except ValueError:
        return None


def trial_name(data: dict, trial_dir: Path) -> str:
    for key in ("task_name",):
        if isinstance(data.get(key), str):
            return data[key]
    task_id = data.get("task_id")
    if isinstance(task_id, dict) and isinstance(task_id.get("name"), str):
        return task_id["name"]
    return trial_dir.name.split("__", 1)[0]


# ---------------------------------------------------------------- report


def tail(path: Path, chars: int) -> str:
    if not path.is_file():
        return "(missing)"
    text = path.read_text(errors="replace")
    return text if len(text) <= chars else "…" + text[-chars:]


def write_report(gen: int, summary: dict, trials: list[dict], job_dir: Path, best: dict) -> Path:
    lines = [f"# Generation {gen}", "",
             f"Score {summary['score']:.3f} (95% CI {summary['ci_low']:.3f}–{summary['ci_high']:.3f}), "
             f"pass@1 {summary['pass1']:.3f}, pass@k {summary['passk']:.3f} over {summary['trials']} trial(s) of {summary['tasks']} task(s). "
             f"Best kept so far: {best.get('score', 0):.3f} (lower bound {best.get('ci_low', 0):.3f}).", ""]
    lines += ["| task | attempts |", "|---|---|"]
    for task, rewards in summary["rewards"].items():
        lines.append(f"| {task} | {' '.join(f'{r:.1f}' for r in rewards)} |")
    lines.append("")
    for t in trials:
        if t["reward"] >= 1.0:
            continue
        trial = job_dir / t["trial"]
        lines += [f"## Failed: {t['task']} ({t['trial']})", ""]
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


# ---------------------------------------------------------------- improve


def improve(args, report: Path, gen: int) -> None:
    note = report.parent / "improvement.md"
    task = META_TASK.format(report=report, mutable=", ".join(MUTABLE), note=note, forbidden=", ".join(META_FORBIDDEN))
    host_bin = os.environ.get("RIGCODER_HOST_BIN") or str(ROOT / "target" / "release" / "rigcoder")
    if not Path(host_bin).is_file() and not args.dry_run:
        sh(["cargo", "build", "--release", "-p", "rigcoder-cli"], dry=args.dry_run)
    cmd = [host_bin, "--cwd", str(ROOT), "--max-turns", "80", "--timeout-secs", "1800",
           "--transcript", str(report.parent / "improve-transcript.jsonl"), task]
    env = {"RIGCODER_PROVIDER": args.meta_provider}
    if args.meta_model:
        env["RIGCODER_MODEL"] = args.meta_model
    sh(cmd, dry=args.dry_run, env=env, check=False)
    if args.dry_run:
        return
    # Whatever the meta-run did outside the mutable set is undone.
    changed = git_out("diff", "--name-only").split()
    outside = [f for f in changed if f not in MUTABLE]
    if outside:
        print(f"meta agent touched non-mutable files, reverting: {outside}", flush=True)
        git(["checkout", "--", *outside], dry=False)
    # Refuse a change that does not compile: revert to the last kept state.
    if subprocess.run(["cargo", "check", "--workspace"], cwd=ROOT).returncode != 0:
        print("improvement does not compile; reverting", flush=True)
        git(["checkout", "--", *MUTABLE], dry=False)


# ---------------------------------------------------------------- ledger


def best_kept() -> dict:
    """The best kept dev-slice generation so far, from the ledger."""
    best: dict = {"score": -1.0, "ci_low": -1.0}
    if LEDGER.is_file():
        for line in LEDGER.read_text().splitlines():
            entry = json.loads(line)
            if entry.get("slice") == "dev" and entry.get("decision") in ("kept", "tie") and entry.get("ci_low", -1) >= best["ci_low"]:
                best = entry
    return best


def record(entry: dict, dry: bool) -> None:
    print(json.dumps(entry), flush=True)
    if not dry:
        with LEDGER.open("a") as f:
            f.write(json.dumps(entry) + "\n")


# ---------------------------------------------------------------- main


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--generations", type=int, default=3)
    p.add_argument("--dataset", default="terminal-bench@2.0")
    p.add_argument("--slice", default="dev", help="task slice under harness/slices/ (dev or holdout)")
    p.add_argument("-i", "--include", action="append", default=[], help="explicit task names instead of the slice (repeatable)")
    p.add_argument("-x", "--exclude", action="append", default=[])
    p.add_argument("-k", "--attempts", type=int, default=3, help="attempts per task")
    p.add_argument("-m", "--model", default="gemini/gemini-3.8-flash", help="provider/model the benchmarked agent uses")
    p.add_argument("--meta-provider", default="gemini")
    p.add_argument("--meta-model", default=None, help="model the improvement step uses (default: provider default)")
    p.add_argument("-n", "--n-concurrent", type=int, default=4)
    p.add_argument("--no-build", action="store_true")
    p.add_argument("--force-build", action=argparse.BooleanOptionalAction, default=True,
                   help="harbor --force-build: build task environments natively instead of pulling amd64 images")
    p.add_argument("--no-improve", action="store_true", help="evaluate only")
    p.add_argument("--holdout", action=argparse.BooleanOptionalAction, default=True,
                   help="score the best kept generation on the holdout slice at the end")
    p.add_argument("--dry-run", action="store_true")
    args = p.parse_args()

    RUNS.mkdir(parents=True, exist_ok=True)
    tasks = args.include or read_slice(args.slice)
    if args.slice == "holdout" and not args.no_improve:
        raise SystemExit("the holdout slice is for evaluation only: pass --no-improve")
    # Self-edits are committed on `evolve`; an evaluation-only run stays on
    # whatever branch it was started from.
    if not args.no_improve and git_out("rev-parse", "--abbrev-ref", "HEAD") != "evolve" and not args.dry_run:
        git(["checkout", "-B", "evolve"], dry=False)
    best = best_kept()

    for gen in range(args.generations):
        build(args)
        trials, job_dir = evaluate(args, f"gen-{gen:03d}", tasks, args.attempts)
        summary = summarize(trials)
        decision = keep_decision(summary["score"], summary["ci_low"], best["score"], best["ci_low"]) if not args.dry_run else "kept"
        head = git_out("rev-parse", "--short", "HEAD")
        entry = {"generation": gen, "slice": args.slice if not args.include else "custom", "commit": head, "decision": decision,
                 "best_score": best["score"], "best_ci_low": best["ci_low"], "model": args.model, "attempts": args.attempts,
                 "job_dir": str(job_dir), "time": time.time(), "per_trial": trials, **summary}
        if decision in ("kept", "tie"):
            best = entry
            if not args.dry_run and git_out("status", "--porcelain", *MUTABLE):
                git(["add", *MUTABLE], dry=False)
                git(["commit", "-q", "-m", f"evolve: generation {gen} scored {summary['score']:.3f} [{summary['ci_low']:.3f}, {summary['ci_high']:.3f}] ({decision})"], dry=False)
                entry["commit"] = git_out("rev-parse", "--short", "HEAD")
        else:
            print(f"generation {gen}: {summary['score']:.3f} [{summary['ci_low']:.3f}] < best {best['score']:.3f} [{best['ci_low']:.3f}]; reverting", flush=True)
            git(["checkout", "--", *MUTABLE], dry=args.dry_run)
        record(entry, args.dry_run)
        if args.no_improve or gen == args.generations - 1:
            continue
        report = write_report(gen, summary, trials, job_dir, best) if not args.dry_run else job_dir / "report.md"
        improve(args, report, gen)

    if args.holdout and args.slice == "dev" and not args.include:
        holdout = read_slice("holdout")
        trials, job_dir = evaluate(args, "holdout", holdout, args.attempts)
        summary = summarize(trials)
        record({"generation": None, "slice": "holdout", "commit": git_out("rev-parse", "--short", "HEAD"), "decision": None,
                "model": args.model, "attempts": args.attempts, "job_dir": str(job_dir), "time": time.time(),
                "per_trial": trials, **summary}, args.dry_run)
    return 0


if __name__ == "__main__":
    sys.exit(main())
