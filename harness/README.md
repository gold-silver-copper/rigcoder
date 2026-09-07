# The improvement harness

Runs rigcoder against Terminal-Bench 2.0 task directories through Docker,
and lets rigcoder edit itself between generations. All of it is Rust:
`crates/rigcoder-bench`, binary `rigcoder-bench`. No benchmark framework
sits in between: a trial is six `docker` calls.

## Pieces

- `crates/rigcoder-bench`:
  - `run`: evaluate the current Linux binary on a slice. Per task: build the
    image from `environment/Dockerfile` (natively, so the arm64 binary matches
    an arm64 container), start a container with the task's `cpus`/`memory`,
    copy the binary and `tests/` in, run the agent on `instruction.md` in the
    Dockerfile's `WORKDIR` under the task's agent timeout, run
    `bash /tests/test.sh` under the verifier timeout, read
    `/logs/verifier/reward.txt`, copy `/logs/agent` and `/logs/verifier` out.
  - `iterate`: the loop. Build, evaluate with `-k` attempts, keep or revert,
    let rigcoder edit itself, repeat; then score the best kept generation on
    the holdout slice.
  - `ledger`: the ledger as a table. `summarize <job>`: one job's metrics.
- `build-linux.sh`: cross-builds the Linux `rigcoder` in a `rust:1.95` Docker
  container into `harness/bin/` (gitignored).
- `slices/dev.txt` (20 tasks) and `slices/holdout.txt` (10, disjoint): the
  task sets. The loop optimizes on dev; holdout is the check that it learned
  something general, and the meta agent is told never to read it.
- `tasks/` (gitignored): a checkout of the dataset,
  `git clone https://github.com/laude-institute/terminal-bench-2 harness/tasks`.
- `ledger.jsonl`: one JSON object per evaluation.

## The score and the keep rule

Evaluation runs `-k` attempts per task (default 3) and reports the mean
reward, pass@1 (mean over tasks of the fraction of attempts passing), pass@k
(a task counts if any attempt passed) and a 95% Wilson interval on the
per-trial pass rate. A generation is kept when its lower bound is at least
the best kept lower bound and its mean has not dropped; a tie keeps and is
recorded as one; anything else reverts the mutable files. Unit tests in
`crates/rigcoder-bench/src/stats.rs` pin the rule.

Every trial records tokens (from rigcoder's `usage` transcript event), tool
calls, wall time and any harness-side error, in `result.json` and summed on
the ledger line.

## The improve step

Runs the host `rigcoder` (`target/release/rigcoder`) on this repository with
a meta-task and `report.md` from the failed trials. It may only edit the
files in `MUTABLE` (the prompt, the tools, the agent settings, the
transcript shaping, the CLI); anything else it touches is reverted, and a
change that does not `cargo check` is reverted. Kept generations are
commits on the `evolve` branch with the note in `improvement.md` beside the
job.

## Running on macOS with colima

If the docker CLI config points at Docker Desktop's credential helper, give
the runner an empty config plus the colima socket:

```sh
colima start pi --cpu 8 --memory 16
mkdir -p /tmp/dockercfg && echo '{"cliPluginsExtraDirs":["/opt/homebrew/lib/docker/cli-plugins"]}' > /tmp/dockercfg/config.json
export DOCKER_CONFIG=/tmp/dockercfg DOCKER_HOST=unix://$HOME/.colima/pi/docker.sock
export GEMINI_API_KEY=...
./harness/build-linux.sh
cargo build --release -p rigcoder-bench -p rigcoder-cli
# baseline on the dev slice, 3 attempts per task, no self-edit:
./target/release/rigcoder-bench iterate --generations 1 --no-improve -k 3 --no-build
# then let it iterate:
./target/release/rigcoder-bench iterate --generations 5 -k 3
```

Only the benchmarked provider's key is passed into containers, and it is
never printed. Results land in `harness/runs/<job>/<task>__<attempt>/` with
`agent/`, `verifier/`, `instruction.md` and `result.json`, plus
`summary.json`, `report.md` and `improvement.md` per job.

## History

The first evolve run (Harbor-based, since replaced by this runner; one
attempt per task, six tasks, gemini-3.8-flash) went 0.833 → 0.833 → 1.0.
Its self-edits fixed a real bug in the bash tool (a timed-out pipeline left
children holding the pipes; now the process group is killed) and taught
the agent that a text-only reply ends the run. The earlier four-task
baselines: gemini-3.1-pro-preview and gemini-3.8-flash both 4/4.
