# Benchmark ladder — a restartable, self-updating run prompt

You are improving rigcoder by climbing a ladder of coding benchmarks, easiest first, through Harbor. This file is both your instructions and your memory: the **STATE** section at the bottom is yours to edit. Every time you are started with this file, read STATE first, resume from it, and keep it current as you go — it must always be safe to kill you and start you again from this file alone. Never rewrite the instruction part above STATE; append lessons to STATE › notes instead.

## Ground rules (never change these)

- Read `harness/ITERATE_PROMPT.md` §0–§4 first; its rules apply (report only what ran here, re-run a failing test alone before calling it real, section-0 checks before any paid run and after every commit, never touch tasks/verifiers/timeouts, never read `harness/slices/holdout.txt` results).
- Runner: Harbor 0.22 with `harness/rigcoder_agent.py` (`PYTHONPATH=. harbor run -a harness.rigcoder_agent:RigcoderAgent …`, `--force-build` on this arm64 host). `rigcoder-bench` stays for `digest`, `replay`, `branch-from` and the ledger; do not add a third runner.
- Model `gemini/gemini-3.8-flash` via `--ae GEMINI_API_KEY=$GEMINI_API_KEY`; never print the key. Record every Harbor job in `harness/ledger.jsonl` (slice `custom`, `meta_commit` = `harbor-<dataset>@<version>`, tokens from the trial `result.json`s) as soon as the job ends, before analysis.
- Rebuild `harness/bin/rigcoder-linux-aarch64` with `harness/build-linux.sh` whenever the pin, prompt or agent source changed since the receipt in `harness/bin/*.build.json`.
- Network: run every rung on a job-owned snapshot of the dataset with `[agent] network_mode = "allowlist"`, `allowed_hosts = ["generativelanguage.googleapis.com"]` added to each task.toml (`harbor datasets download` into `harness/runs/datasets/<name>@<version>/`, then apply the policy with a small script committed as `harness/harbor_policy.py`; the registry copy is never edited). Verifier phase stays at the task's baseline. Record the probe (`RIGCODER_NETWORK_PROBE=1`) on the first trial of every rung.
- Budget: STATE › budget is the hard cap for the whole ladder. Before each rung estimate cost from the previous rung's per-trial mean (or the estimate in the table) and stop with a report if the rung would cross the cap. Unknown usage is never zero: a trial with no settlement is reserved at the rung's max observed trial cost.
- Commit each fix on its own; Conventional Commit form; no batched unrelated changes. Never commit `harness/runs/`.

## The ladder

Run rungs in order. A rung is **done** when the whole dataset has run at k=1 (n=4, 200 turns), its ledger row exists, its analysis is written, and every rigcoder bug it exposed is fixed and re-verified on the affected tasks. Do not skip a rung or subset it to make it pass; if the estimate exceeds budget, stop and report.

| # | dataset@version | tasks | est. $/trial | why it is here |
|---|---|---:|---:|---|
| 1 | `hello-world@1.0` | 1 | 0.05 | adapter and policy sanity |
| 2 | `terminal-bench-sample@2.0` | 10 | 1.0 | our own genre, leaderboard format |
| 3 | `quixbugs@1.0` | 80 | 0.15 | tiny single-function fixes; exercises edit_file |
| 4 | `humanevalfix@1.0` | 164 | 0.15 | same, more languages |
| 5 | `aider-polyglot@1.0` | 225 | 0.25 | multi-language edits with tests |
| 6 | `livecodebench@6.0` | 100 | 0.30 | reasoning-heavy generation |
| 7 | `bigcodebench-hard-complete@1.0.0` | 145 | 0.30 | library-heavy Python |
| 8 | `terminal-bench@2.0` | 89 | 1.75 | our full benchmark, all tasks, leaderboard format |
| 9 | `terminal-bench-pro@1.0` | 200 | 2.0 | harder terminal tasks |
| 10 | `swebench-verified@1.0` | 500 | 2.5 | repo-level bug fixing |

## Per-rung procedure

1. **Prepare.** Section-0 checks green. Binary receipt matches HEAD. Snapshot + policy applied. `--install-only` on one task of the rung passes.
2. **Run** the whole rung: `harbor run -p <snapshot> -a harness.rigcoder_agent:RigcoderAgent -m gemini/gemini-3.8-flash --force-build -n 4 -o harness/runs --job-name ladder-<n>-<dataset>`. Retries: none (`--max-retries 0`); a trial that errors is evidence.
3. **Ledger** the job immediately.
4. **Analyse every trial**, not just failures. For each trial dir read, in this order: `result.json` (reward, exception), `agent/transcript.jsonl`, `agent/effects.json` (every model exchange and tool call, replayable), `agent/observations.json` (holds, denials, approvals, retries, truncations, endings), `agent/rigcoder.txt`, `verifier/`. Use `rigcoder-bench digest <job>` for the counts and the contamination column, then read the transcripts the digest points at. Put every failed or errored trial in exactly one bucket — `infra`, `harness`, `rig`, `model` (definitions in ITERATE_PROMPT §3) — and record it in STATE › findings with trial path, transcript line, bucket and one-line mechanism. Also record the top three cost outliers among passes and what they spent it on. Signs of a `rig` bug: mis-serialised tool arguments, dropped or mis-assembled stream chunks, unparsed provider fields, wrong error kinds, replay/effect-bus divergence, panics — check them against the effect log, and reproduce with `rigcoder-bench replay <trial>` or an offline `rigcoder-verify` case before believing the transcript.
5. **Fix rigcoder bugs** (`harness` bucket): the most frequent mechanism first, one change per commit; before paying, `replay` the affected trials or `branch-from` a checkpoint; then re-run only the affected tasks of the rung through Harbor (`-i <task>`), and record before/after per task in STATE. Keep only by the repo's rule (no new harness failures, mean not down, Wilson lower bound not below the best kept).
6. **Rig bugs** (`rig` bucket): follow ITERATE_PROMPT §4 exactly — minimal failing Rust test against the pin; check whether Rig `main` already fixes it (`git ls-remote https://github.com/0xPlaygrounds/rig refs/heads/main`, full clone in the scratchpad); if it does, move the pin by the §4 procedure and continue. Otherwise branch `fix/<short>` from `main`, fix + test, get the crate's tests/clippy/fmt and `cargo xtask verify --changed` green, commit, `gh pr create` against `main` with the `other.md` template sections, point every Rig dep in `Cargo.toml` at the branch commit with the `# TODO: return to Rig main when <PR URL> merges` comment, record the PR in `harness/UPSTREAM.md`, rebuild, re-run the affected tasks against the branch pin, record before/after. Then **set STATE › blocked_on to the PR URL and stop** with a report. Do not continue the ladder on a branch pin.
7. **Rung report**: append to STATE › rungs: pass count, mean reward, Wilson interval, cost, cost per resolved trial, taxonomy counts, bugs fixed with commits, and the two or three most useful observations. Then advance `current_rung`.

## On (re)start

1. `git status`, `git log --oneline -5`, current pin in `Cargo.toml`, receipt in `harness/bin/`.
2. If STATE › blocked_on is a PR URL: `gh pr view <url> --json state,mergeCommit`. If merged: move the pin to the merge (or `main`, §4 pin-move procedure), clear `blocked_on`, re-run the affected tasks on the merged pin, record it, then continue the rung. If not merged: check the PR for review comments, address them in the branch if they are ours to address, and stop again with a one-paragraph status. Never restart a rung from scratch while blocked.
3. If a job in STATE › in_progress has no ledger row, look for its `result.json` under `harness/runs/`; ledger it if complete, otherwise treat its trials as unknown-cost and note it.
4. Resume at `current_rung` step `current_step`.

## Stop conditions (report and stop)

A Rig PR opened; budget would be exceeded by the next rung; the same harness failure mode survives two fixes; more than a third of a rung's failures are `infra`; a pin move breaks more than fixtures and a handful of call sites; any holdout exposure; the adapter or egress policy is not enforcing (probe not denied). The report: taxonomy per rung, commits with per-task deltas, cost per resolved trial trend, open upstream PRs, current pin, and what the next start should do.

---

# STATE (edit below this line only)

```yaml
version: 1
budget_usd_cap: 1000
spent_usd_known: 0.00
spent_usd_reserved_unknown: 0.00
current_rung: 1
current_step: prepare      # prepare | run | ledger | analyse | fix | report
in_progress_job: null      # harness/runs/<job> while a run is live
blocked_on: null           # Rig PR URL, or null
pin: 234626eb8e75ee8af76e364012e3fd358119360b
binary_receipt_head: null  # source_head from harness/bin/*.build.json at last build
last_started: null         # ISO date of the last (re)start
```

## rungs
<!-- one entry per finished rung:
### <n> <dataset@version> — <date>
job: harness/runs/<job>  passed: x/y  mean: 0.xx  wilson: [a, b]  cost: $x  per-resolved: $x
taxonomy: infra n · harness n · rig n · model n · contaminated n
fixes: <commit> <one line> (task: before→after)
observations: - … -->

## findings
<!-- every failed/errored trial, once: <job>/<trial> L<line> — <bucket> — <mechanism>; and cost outliers -->

## upstream
<!-- PR URL — what — state — pin commit used while open -->

## notes
<!-- lessons for the next start: adapter quirks, dataset quirks, things that wasted money -->
