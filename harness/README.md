# The improvement harness

Runs rigcoder against Terminal-Bench 2.0 task directories through Docker,
and lets rigcoder edit itself between generations. All of it is Rust:
`crates/rigcoder-bench`, binary `rigcoder-bench`. No benchmark framework
sits in between; the runner controls each Docker step.

The separate [ECS consumer harness](../crates/rigcoder-verify/src/consumer/README.md)
owns the verification corpus transferred from Rig, including the deliberately
broken `harness/repair-project`. Run its 42-case offline matrix with
`cargo run --locked -p rigcoder-verify -- verify`. It retains its own consumer
tools, fixtures, approval/repair workflow, replay and resume implementation;
it is not a new acceptance gate in this benchmark improvement loop.

## Pieces

- `crates/rigcoder-bench`:
  - `run`: evaluate the current Linux binary on a slice. Per task: build the
    image from `environment/Dockerfile` (natively, so the arm64 binary matches
    an arm64 container), start a container with the task's `cpus`/`memory`,
    copy the binary in, run the agent on `instruction.md` in the
    Dockerfile's `WORKDIR` under the task's agent timeout, then install a fresh
    `tests/` and run
    `bash /tests/test.sh` under the verifier timeout, read
    `/logs/verifier/reward.txt`, copy `/logs/agent` and `/logs/verifier` out.
  - `iterate`: the loop. Build, evaluate with `-k` attempts, keep or revert,
    let rigcoder edit itself, repeat; then score the best kept generation on
    the holdout slice.
  - `ledger`: the ledger as a table. `summarize <job>`: one job's metrics.
  - `digest <job>`: the failure digest (what failed trials did more of than
    passed ones), also written as `digest.json`.
  - `replay <trial> [--prompt-file P]`: replay a recorded trial on this
    machine through the host `rigcoder`: the model and the tools answer from
    `agent/effects.json`, nothing is called and nothing is written. Exit 0
    means the current prompt and tools reproduce the recorded requests; exit
    3 reports an identity or request divergence. That is the cheap
    check for "did this edit change the trajectory" before paying for a run.
  - `branch-from <trial> <turn> [--times N]`: resume a trial recorded with
    `run --checkpoint` from that turn in fresh containers (workspace restored
    from the checkpoint tarball, scene loaded) and count verifier passes: the
    fast inner loop for one failed trial.
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

An incomplete evaluation fails without publishing a score. Image builds, artifact
copies, verifier exits and finite rewards in `0..=1` are checked; stale verifier
files are cleared after the agent stops. Every job gets an exclusive directory.
Each iterate invocation measures a fresh baseline for its provider, model and
slice instead of comparing against unrelated historical ledger rows.

Every trial records tokens (from rigcoder's `usage` transcript event), tool
calls, wall time and any harness-side error, in `result.json` and summed on
the ledger line.

## Steering systems

rigcoder's tool calls are effect entities, so three small systems in
`crates/rigcoder/src/steer.rs` do what hooks would: a `BusSet::Gate` system
denies bash commands on a deny list (`find /`, recursive greps of `/`,
`rm -rf /`) with a reason the model reads, or holds ones on a hold list for
approval (automatic in the CLI, `y`/`n` in the TUI); a `BusSet::Judge` system
cuts over-long tool results to head and tail for history while the record
keeps the handler output and the result keeps its success/failure status. A
`RigSet::Judge` system turns a text-only answer while a `--deliverable` file is
missing into a retry naming the files. Handler output can itself be bounded:
bash drains stdout/stderr into fixed
head/tail captures, and large reads return an outline. On Unix, shell deadlines
and cancellation terminate the process group and stop readers even if a detached
descendant holds a pipe open; deliberately detached processes are not a sandboxed
process tree. Invalid steering regexes fail closed, and cancelled approval holds
are removed. The rules live in the `Steer` resource (the settings lane); the systems are the
systems lane. `crates/rigcoder/tests/steer.rs` pins them.

Transient provider failures may retry the current prompt up to three times,
with backoff, only before that request has produced tool work. A new user request
resets the retry budget. Backoff remains busy and can be cancelled; effect-log
publication waits for all attempts and fails the CLI if the requested file cannot
be written. Tests cover failure after a tool, cancellation during backoff and
record/replay across failed and successful attempts.

## Recording, replay, checkpoints

Every fresh run can write an effect log (`--effect-log`, always on in trials).
Replay validates the saved settings, tool/source and steering-policy fingerprint,
as well as every recorded request, before reporting success. It binds recorded
handlers in grant order and checks replayability. The log stores the complete
handler output before presentation shaping, subject to each tool's own capture
limits. Exact replay is a trajectory diagnostic; it does not test a changed tool
implementation by executing it.

`--checkpoint DIR` saves the run graph after completed tool batches and at terminal
settlement. Resume recounts the materialized turns and continues a saved graph
without reissuing completed work. `--checkpoint-tar` also snapshots the workspace;
its checkpoint directory must be outside that workspace to avoid self-inclusion.
A scene-only directory may be inside it. Archive, scene and write failures fail
the run; existing snapshots cannot be overwritten with a new run's reused names.
`--resume SCENE` restores the graph, while the caller must restore the matching
workspace separately. `branch-from` handles that restoration in a fresh container
and refuses system-root, traversal and symlink workspace destinations.

Logs written after `--resume` contain continuation effects, not the original
prefix; they are not standalone inputs to plain `--replay`. A scene also does not
persist all host resources or provide an exactly-once filesystem transaction.
`crates/rigcoder/tests/replay.rs` covers replay and checkpoint/resume behavior.

## The improve step

Runs the host `rigcoder` (`target/release/rigcoder`) on this repository with
a meta-task, `report.md` from the failed trials, and one lane (prompt, tools,
settings, shaping or systems) chosen from the digest or forced with `--lane`.
The lane is enforced before dispatch using canonical write/edit paths and an
explicit note-file exception. Bash is disabled while `Scope` is active; reads
remain available. The trusted harness runs `cargo check` after cleaning changes.

Tracked, staged and untracked changes outside the lane are restored after the
agent exits, including staged changes masked by matching working-tree contents.
The generated ledger is preserved. These are recovery safeguards rather than
an isolation boundary for ignored files or arbitrary host code. Self-improvement
requires a clean index and worktree, except the generated unstaged ledger. It
creates `evolve` when absent or resumes it, never resets an existing branch,
and stops for inspection if the agent changes HEAD or the symbolic branch.

Kept generations commit actual changed lane files and the current note, without
requiring optional directories to exist. Committed notes remain in
`harness/notes/`; rejected notes remain with their job artifacts. The host binary
is rebuilt after accepted edits, and `meta_commit` identifies the improver.
Publication is separate from iteration: after a frozen baseline/candidate
holdout comparison and repository verification/review, publish through the
normal repository workflow. The loop does not push branches or open PRs.
Development keep/tie decisions are exploratory ranking, not promotion evidence.
The improve step records its own effect log and scene checkpoints.

Task timeouts must be finite, representable durations of at least one second.
Execution uses whole seconds, rounding fractional values down. Zero, negative,
subsecond and nonfinite values are rejected before task builds or trials so
they cannot become GNU timeout's zero-duration “disable deadline” setting.

`--no-improve` preserves branch, commits, index and user edits. `--no-build` is
for evaluation only: self-edits and the final holdout rebuild the current source.
Built binaries use `harness/bin/rigcoder-linux-aarch64` or `-x86_64`, defaulting
to the host architecture. Custom binaries require `--no-build`.

The Linux build requires Python 3 and copies Cargo manifests, the lockfile,
toolchain file and `crates/` into a temporary source tree. Docker mounts that tree
read-only, with separate output and Cargo caches. The harness, dataset and Git
history are absent from the build mount; links and special input files are
rejected. The build uses `--locked` and writes a `.build.json` receipt beside the
binary with captured input hashes and the output hash. Failed builds remove both
outputs. Normal evaluation requires a receipt matching its binary snapshot,
current source inputs, build command and architecture. The manifest retains the
receipt as `matched_receipt`; it is trusted build bookkeeping, not a signed
attestation. The wrapper pulls the requested platform, resolves its builder image ID and
uses that immutable ID for both execution and the receipt.
Explicit `--no-build` evaluation without a receipt remains `unverified`; a
present but mismatched receipt is rejected. Run the wrapper's tests with
`python3 -B -m unittest discover -s harness -p test_build_inputs.py`.

## Running on macOS with colima

Install GNU coreutils (`brew install coreutils`) for host image-build and
execution deadlines. The runner accepts GNU `timeout` or Homebrew's `gtimeout` on `PATH`
and checks this dependency before starting. Containers need their own GNU
`timeout` command as well. The host deadline bounds the Docker execution client
even if the agent replaces the container's timeout. It does not terminate remote
processes by itself; trial cleanup removes the container. This is not a trusted
scoring boundary, and other Docker operations still depend on daemon responsiveness.

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
never printed. Results land in `harness/runs/<prefix>-<label>-<unique-id>/<task>__<attempt>/` with
`agent/`, `verifier/`, `instruction.md` and `result.json`, plus
`summary.json`, `report.md` and `improvement.md` per job. `report.md` stays
within 400 KiB; oversized evidence is preserved in linked `report.full.md`, with
a bounded UTF-8 head/tail excerpt for the meta agent. These task containers run
the agent as root and do not isolate against a malicious agent.

## History

The first evolve run (Harbor-based, since replaced by this runner; one
attempt per task, six tasks, gemini-3.8-flash) went 0.833 → 0.833 → 1.0.
Its self-edits fixed a real bug in the bash tool (a timed-out pipeline left
children holding the pipes; now the process group is killed) and taught
the agent that a text-only reply ends the run. The earlier four-task
baselines: gemini-3.1-pro-preview and gemini-3.8-flash both 4/4.
