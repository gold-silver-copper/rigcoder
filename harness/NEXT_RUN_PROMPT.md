# Task: the next rigcoder iteration run

The rules are `harness/ITERATE_PROMPT.md`; read it first, then this
file, then `harness/NOTES.md` from "Dev tier (2026-09-12)" to the end.
This file says where things stand and what this run is for.

## Where things stand (2026-09-13)

- Rig pin: `3c4346318198f92583631936d73dd8d3e7f392ce`, Rig `main` after
  #2443, #2500 (run-level provider retries), #2501 (canonical part
  order) and #2502 (scrub helpers, retryable truncation). No open Rig
  PRs. `harness/UPSTREAM.md` has the history.
- Landed since the dev tier was measured (`dev-tier-run-dev-1789258759508791000-59132`,
  50/60, pass@1 0.833, $103.81, on commit `d6f7787`):
  - `02fcc82` provider failures retry inside the run (the session no
    longer resubmits prompts; `RunSettings.provider_retries` is the run's
    `ProviderRetries` budget, default 3; backoff is a Gate hold).
  - `e606cd5` the deliverables audit removes build outputs the task did
    not ask for (polyglot-rust-c's 3-of-6 failure mode).
  - `9fb55e4` grep and list_files never walk `/` or a pseudo-filesystem;
    grep skips files over 4 MiB (headless-terminal was killed at the
    memory limit walking `/`).
- Smoke is clean: last run `c4-walk-run-smoke-1789279457499080000-67562`
  10/10, $17.62. None of the three fixes has been measured on the dev
  tier yet; that is the first job here.
- Not built: commit 2 of `HARNESS_FIX_PROMPT.md`, the byte-identical
  repeat cutoff. Measured against the five most expensive recorded
  trials it would have denied 0 to 5 cheap `ls` calls and left every
  spiral intact (NOTES, "Commit 2 ... not built"). Do not build it.
- Budget: the cap is $400; spend to date $169.80. A smoke run is $9 to
  $18, a dev run about $105 to $120. Plan on one dev run, one or two
  smoke runs, and a second dev run only if a spiral budget lands.

## What this run is for, in order

### 1. Measure the three fixes on the dev tier

`rigcoder-bench run --slice dev -k 3 -n 4 --no-build --job-name dev2`
on the current binary (`harness/build-linux.sh` first; confirm the
build manifest's `source_head` is the commit you are on). Pair per task
against the first dev run's `result.json` files, not its aggregate.
Threshold, written to NOTES before launch:

- polyglot-rust-c: 3/3 expected (was 1/3); fewer than 2/3 means the
  audit line is not enough and the prompt lane is the next lever.
- fix-code-vulnerability and password-recovery: no trial may end on a
  provider failure with `retryable: true` in its transcript's `failed`
  reason; a block for `OTHER` or a 503 must show as `retrying` lines
  and a `rig-ecs/agent/provider_retry` fact, with the tool count
  unchanged across the retry. Their pass counts were 1/3 and 2/3.
- No trial may end with exit 137 or `settled: false` and no `failed`
  event; that was the walker kill.
- Everything else: a task that was 3/3 must stay at least 2/3; a
  1-task delta elsewhere is noise. Cost per resolved task is reported
  beside pass@1; the run is expected to cost about the same as the
  first because nothing here touches the spiral tail yet.

Classify every failure into the four buckets as before. Expected
`model` residue: configure-git-webserver (0/3 both times, the deploy
hook leaves 404). Do not fix it.

### 2. The spiral tail: design before code

The cost tail is trials of 119 to 194 calls (db-wal-recovery,
make-mips-interpreter) at $8 to $12 each; one passed, three failed.
Their calls are mostly distinct, so "identical call repeated" is the
wrong signal. Using the dev2 transcripts and the three recorded spirals
(`harness/runs/dev-tier-*/db-wal-recovery__3`,
`harness/runs/c4-walk-*/db-wal-recovery__1`,
`harness/runs/dev-tier-*/make-mips-interpreter__1`), measure at least
these candidate signals before choosing one, and write the numbers to
NOTES:

- calls since the last `write_file`/`edit_file` (or bash that changed
  the workspace; the effect log's tool outcomes say);
- calls since the last tool result that differed from every earlier
  result of the same tool (a probe that returns what it already
  returned);
- cumulative input tokens or dollars for the run (the transcript's
  `usage` event is per run; the effect log has per-attempt usage).

For each, report the earliest point at which a cutoff would have fired
on each spiral and whether it would have fired on any passing trial in
dev2 (a false positive costs a pass). Choose the signal with the
best margin; if none separates the spirals from the passes, stop and
report the table instead of building anything.

Then implement it as a `Steer` rule in `crates/rigcoder/src/steer.rs`,
default on, with the action being a run-level ending, not a denial:
the run ends `Failed(Unsupported("spiral: <signal> exceeded <n>"))`
through a `RigSet::Judge` system that inserts `Cancelled` on the run
with that reason, so the transcript's `failed` line and the witness
name the rule, and the digest counts it under a new fact
`rigcoder/spiral_stop`. A denied call would only send the model back
for another probe. Unit tests replay the three recorded call sequences
against the rule and assert where it fires; `validate` rejects a zero
budget. Evidence packets regenerate (steer.rs is in the policy hash).

Paired smoke first (threshold: no passing task lost; the rule must not
fire on smoke, where the longest trial is 108 calls), then dev2 vs a
third dev run if the cap allows: the claim is cost per resolved task
down with pass@1 within noise. Say plainly if only smoke ran.

### 3. If step 1 shows the retry not working

Provider retries are pinned by unit and cassette tests, not by a live
trial. If a dev2 trial ends on a retryable failure without `retrying`
lines, that is a `harness` bug ahead of step 2: reproduce it with the
trial's `effects.json` through `rigcoder-bench replay` (the policy hash
matches as long as you change no source), fix in `session.rs`, and pair
on smoke.

## Things learned today that the rules did not say

- Cassette re-recording: a cell whose recorded request no longer matches
  after a Rig change is re-recorded with `RIG_PROVIDER_TEST_MODE=record`
  and `GEMINI_API_KEY`, and then its packet is regenerated again in
  replay mode; a packet written in record mode carries live signatures
  and drifts.
- Every edit to a source in the policy hash (`lib.rs`, `model.rs`,
  `checkpoint.rs`, `tools.rs`, `file_change.rs`, `approval.rs`,
  `steer.rs`, `session.rs`, the settings) moves every cell; regenerate
  once after the last source edit, not after each one. `prompt.md` is
  not in the cells' identity (they run under a prompt override).
- `rigcoder-verify` fixtures move with the Rig identity: derive every
  case, inspect the aggregated diff, then promote with a loop that
  word-splits (`${=line}` in zsh; a quoted line is one argument).
- A test that scripts one retryable failure and asserts the run ends on
  it now retries under the default budget; give its agent
  `ProviderRetries(0)` and say so in the doc comment.
- Rig's `xtask verify --changed` does not run the root integration
  suite; before opening a Rig PR run the CI selection,
  `cargo nextest run --locked --features bedrock --retries 2 -E 'not binary(macro_hygiene)'`,
  and expect `llamacpp::cassette::loaders::loaders_smoke` to fail in a
  clone under `/private/tmp` (the scrubber redacts only home paths).
- Rig `main` can go red on its own: #2501 merged with goldens hashed
  before #2500. Check `main`'s last CI run before blaming a PR, and
  rebase before regenerating goldens.

## Stop conditions, beyond the rules

- Step 1's threshold fails on the retry or the walker: report before
  step 2.
- No spiral signal separates the spirals from the passes.
- Spend would pass $400.

Report: the paired dev table, the taxonomy, the signal table from step
2 with the chosen cutoff and where it fires, commits with per-task
deltas and cost per resolved task, spend, the current pin.
