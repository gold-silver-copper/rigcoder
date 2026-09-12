# Task: iterate on rigcoder

You are improving rigcoder, a coding agent built on `rig-ecs`, which
landed in Rig `main` via PR #2443. Rig is pinned by git revision in
`Cargo.toml`; Rig `main` moves often and with breaking changes, so the
pin never floats and a pin move is its own procedure (section 4). The
repo already contains its own benchmark runner and self-improvement
loop; use them, do not add a second harness. Your job is a fast
eval → diagnose → fix → re-eval loop. Optimize for cycle time. A full
run is not the goal.

## 0. Ground rules

- Report only what ran in this session. A PR body, a notes entry or a
  commit message never claims a test passed, a matrix is green or a
  consumer compiles unless you ran it here and saw the result.
- Before calling a test failure real, re-run that test alone. The
  rigcoder-verify suite has a test that compiles a real project under a
  deadline; it can fail under full-workspace parallel load and pass in
  isolation.
- Fast checks, in this order, before any paid run:
  `cargo check --workspace --all-targets`, `cargo fmt --all`,
  `cargo clippy --workspace --all-targets` (must be warning-free),
  `cargo test --workspace`, then
  `cargo run --locked -p rigcoder-verify -- verify` (42 cases, offline).
- To read pinned Rig source, use cargo's checkout at
  `~/.cargo/git/checkouts/rig-*/<rev prefix>/`; it is complete and fast.
  For Rig history or a Rig branch, use a full clone in the scratchpad,
  not a `--filter=blob:none` clone: `git grep` and `git log -S` on a
  partial clone fetch blobs one at a time and stall.

## 1. Setup (once; verify before continuing)

- Read `README.md`, `harness/README.md`, `crates/rigcoder/src/prompt.md`
  and `crates/rigcoder-bench/src/main.rs`. Do not write a summary; confirm
  in two lines which provider/model you will use and what one trial costs.
- Docker is provided by OrbStack, not Docker Desktop. The `docker` CLI and
  socket work as normal; images build natively as arm64. Do not install or
  start Docker Desktop, and do not switch contexts (`docker context ls`
  should show `orbstack` active).
- Confirm the toolchain and dataset are in place:
  `rustc --version` matches `rust-toolchain.toml`; `harness/tasks/` is a
  checkout of `laude-institute/terminal-bench-2`; `harness/build-linux.sh`
  produces `harness/bin/rigcoder`. Run `cargo run -p rigcoder-bench -- --help`
  and read each subcommand's `--help` before using it.
- Model: `<PROVIDER>/<MODEL>` for smoke and dev (fill in; the ledger's
  recent rows used `gemini/gemini-3.8-flash` through `harness/gemini_gateway.py`).
  Never change the model between paired runs.
- Budget: $<N> per iteration, $<M> total. If unset, ask before the first
  paid run. Every job writes tokens and cost to `harness/ledger.jsonl`.
- Baseline: run the smoke slice once with the current binary before
  changing anything. Its ledger line is the baseline; `harness/runs/` is
  gitignored, so the ledger line is what you commit.

## 2. Slices

- **Smoke (every iteration):** a fixed 10-task subset of
  `harness/slices/dev.txt`, `-k 1`. Write it once to
  `harness/slices/smoke.txt`, commit it, never change it mid-session.
  Purpose: catch breakage (edits not applying, malformed tool calls,
  container/setup failures). The error taxonomy matters more than pass rate.
- **Dev (only with a candidate improvement):** all of `dev.txt` at `-k 3`,
  plus Aider Polyglot via `harness/polyglot_execute.py` and
  `polyglot_evaluate.py`, because Polyglot isolates edit-format correctness.
  Keep or revert by the repo's rule: Wilson lower bound not below the best
  kept, mean not dropped (`crates/rigcoder-bench/src/stats.rs`).
- **Holdout (`harness/slices/holdout.txt`): never run it, never read its
  per-task results.** If you think a milestone warrants it, stop and ask.

## 3. The loop

1. `rigcoder-bench run` on the smoke slice.
2. `rigcoder-bench digest <job>` and `summarize <job>`. From `result.json`
   per trial you need: task, pass/fail, turns, tokens in/out, cost, wall
   clock, tool-call errors, malformed edits, and whether the agent
   recovered after its first failed test run or spiralled. Extend
   `digest` if a column is missing; do not write a parallel parser.
3. Put every failure in exactly one bucket:
   - `infra`: Docker/OrbStack, timeout, network, gateway, the runner itself
   - `harness`: rigcoder code: prompt, edit application, tool dispatch,
     steering, loop control
   - `rig`: a bug or limitation in the pinned Rig revision (section 4)
   - `model`: the model could not do it
   Log `infra` and `model`; never pick them as the fix target.
4. Pick the single most frequent `harness` or `rig` failure mode. Fix that
   one thing. No batched unrelated changes.
5. Before paying for a re-run, use the cheap checks:
   `rigcoder-bench replay <trial>` tells you whether the edit changed the
   recorded trajectory at all; `branch-from <trial> <turn> --times N`
   re-runs one failed trial from its checkpoint. Only then re-run smoke.
6. Compare paired per-task results against the previous job, not
   aggregate pass rate.
7. Commit with: what changed, which failure mode it targeted, before/after
   per-task passes, before/after cost per resolved task.

Rules:
- State the acceptance threshold before running, in `harness/NOTES.md`.
  With 10 tasks at `-k 1`, a 1–2 task delta is noise. Keep a change only
  if it removes the targeted failure mode without adding `harness`
  failures, or if it is a clear correctness fix.
- Cost per resolved task sits next to pass rate in every comparison. A
  pass-rate gain bought with retry fan-out that doubles cost is not an
  improvement; flag it and ask.
- Never edit tasks, verifiers or timeouts. Never touch `holdout.txt`.
- The section 0 checks must be green after every commit.

## 4. When the failure is in `rig`

Rig here is a pinned `main` revision, not a release. If you trace a failure
to Rig (tool-call arguments mis-serialized, stream chunks dropped or
mis-assembled, provider fields unparsed, history handling, panics, wrong
error types, effect-bus or replay defects):

1. Write a minimal failing Rust test against the pinned revision.
2. Check whether Rig `main` has moved and whether its head already fixes
   it. Compare the pin against the head before reading the diff:
   `git ls-remote https://github.com/0xPlaygrounds/rig refs/heads/main`.
   If the head fixes it, move the pin (procedure below) and stop here.
3. Otherwise check open Rig issues and recent PRs for coverage. If an API
   rigcoder uses was removed as "dead" or "no callers", the audit only
   saw the Rig workspace: rigcoder and rigcoder-verify are out-of-tree
   callers (rigcoder-verify was moved out of Rig in #2474). Say so in the
   PR; restore the API rather than rewriting the consumer.
4. You have write access to `0xPlaygrounds/rig`; do not fork. In a full
   clone, branch `fix/<short-description>` from `main`, add the fix and
   the test, and get these green for the affected crate:
   `cargo test -p <crate>`, `cargo clippy -p <crate> --all-targets -- -D warnings`,
   `cargo fmt -p <crate> -- --check`, then `cargo xtask verify --changed`
   at the workspace root (CONTRIBUTING's fast loop).
5. Commit in Conventional Commit form (`fix(<crate>): ...`). Open the PR
   with `gh pr create` against `main` using the sections from
   `.github/PULL_REQUEST_TEMPLATE/other.md`: Description (observed,
   expected, the repro, how it was found), `## Changelog` in the
   `- *(scope)* ...` voice, `## Migration` or `None`, Testing listing
   only commands you ran. Never edit `CHANGELOG.md` or `MIGRATING.md`;
   CI fails the PR if you do.
6. Until it merges, point every Rig dependency in `Cargo.toml` at the
   branch commit with a `# TODO: return to Rig main when <PR URL> merges`
   comment. Record the URL in `harness/UPSTREAM.md`.
7. Re-run smoke to confirm the bucket is gone.

Stop and ask before opening a PR if the fix is a large or opinionated API
change, or would break the effect-bus protocol.

### Moving the Rig pin

A pin move is a migration, not a one-line edit. Expect breaking API
changes and a new effect-log wire shape. In order:

1. Replace the revision in all five Rig deps in `Cargo.toml`, run
   `cargo update -p rig-core -p rig -p rig-cassette -p rig-ecs -p rig-effect-log`,
   and confirm `Cargo.lock` carries the new revision only.
2. Set `RIG_REV` in `crates/rigcoder/tests/gemini_observe/support.rs` to
   the full revision and every evidence `cell.json` `"rig"` label to its
   first 12 characters.
3. `cargo check --workspace --all-targets`. Fix each compile error by
   reading the new API in cargo's checkout, not by guessing; a `Result`
   that a function newly returns gets handled at the call site, never
   discarded with `let _ =`. Handle `#[must_use]` warnings the same way.
4. Run the Gemini observe matrix. Drift panics name the packet; the
   packets are offline replays, so regenerate them all at once with
   `RIGCODER_EVIDENCE=write cargo test -p rigcoder --test gemini_observe`.
   Cells whose assertions fail are not written; fix the assertion, then
   regenerate those cells the same way.
5. Review `git diff --stat -- fixtures` and a normalized histogram of the
   changed lines before accepting them. Expected churn: the `rig` label,
   the `policy` hash, renamed error kinds, batch numbers and clock ticks.
   Anything else is a behaviour change to explain or a bug to fix.
6. Run the full section 0 checks. Update the pin description in
   `README.md`, `docs/observe-minimal-migration.md` and
   `harness/UPSTREAM.md`. Commit the pin move on its own.

## 5. Keep updated

- `harness/NOTES.md`: one terse entry per iteration: taxonomy counts,
  what changed, the paired result, the threshold you set beforehand.
- `harness/UPSTREAM.md`: Rig fixes opened and their status, plus the
  current pin and the date it was taken.
- `harness/slices/smoke.txt`: written once.

## 6. Stop conditions

Stop and report when any of these hit:
- Three consecutive iterations with no `harness` or `rig` failures on
  smoke. Propose moving to dev.
- The same failure mode survives two targeted fixes.
- More than a third of smoke failures are `infra`.
- A Rig fix needs a breaking API change.
- A pin move breaks more than the fixtures and a handful of call sites,
  or removes an API rigcoder-verify's design depends on. Report which
  upstream commit did it and the options; do not rewrite the consumer
  on your own.
- Spend exceeds the budget.

Report: taxonomy counts first vs last run, commits with per-task deltas,
cost per resolved task trend, open upstream PRs, and the current Rig pin.
