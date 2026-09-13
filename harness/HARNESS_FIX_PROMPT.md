# Task: fix the harness-side failure modes from rigcoder's dev tier

You are working in `gold-silver-copper/rigcoder` on a branch from `main`.
The dev tier (20 tasks, k=3, `gemini/gemini-3.8-flash`, job
`dev-tier-run-dev-1789258759508791000-59132`, see `harness/NOTES.md`)
found three things the harness owns. Fix them as three separate
commits, each with its own paired evidence, in this order. The Rig-side
defects (block reason classification, in-run completion retry) are a
separate task in `harness/RIG_FIX_PROMPT.md`; do not work around them
here.

Ground rules are those of `harness/ITERATE_PROMPT.md` section 0: report
only what ran; re-run a failing test alone before calling it real; the
checks in this order before any paid run:
`cargo check --workspace --all-targets`, `cargo fmt --all`,
`cargo clippy --workspace --all-targets` (warning-free),
`cargo test --workspace`, `cargo run --locked -p rigcoder-verify -- verify`.
Any change under `crates/rigcoder/src` moves the policy hash: run
`RIGCODER_EVIDENCE=write cargo test -p rigcoder --test gemini_observe`,
then confirm with `git diff --stat -- fixtures` and a histogram of the
changed lines that only `policy`, `batch` and stream-batching lines
moved. Budget: about $10 per smoke run; ~$58 remained after the dev tier
on 2026-09-12. One paired smoke run per commit; no dev run without asking.

## Commit 1: build outputs left beside a deliverable (prompt)

### Observed

`polyglot-rust-c` failed 3 of 6 trials across smoke and dev with the
verifier line `Expected only main.rs, found: ['main.rs', 'main', 'cmain']`.
The program was correct every time. The agent compiled test binaries
into the deliverable directory and never removed them; its final
summary never mentioned them. No denial, no provider error.

### Where

`crates/rigcoder/src/prompt.md`, the "Pre-completion deliverables audit"
bullet (line 13 and its numbered sub-items). This file is in the
`Prompt` lane of `rigcoder-bench iterate` (`evolve.rs` line 24), so it
is exactly the lever that loop would use.

### Required change

Add one sub-item to the audit, in the same voice as the existing three:
list the deliverable's directory and remove anything you created that
the instruction did not ask for (compiled binaries, scratch files, test
copies), then re-list it. Keep it to two sentences. Do not add general
"clean up" advice elsewhere; the audit is the one place the model reads
right before settling.

### Evidence

- `cargo test -p rigcoder` green; regenerate the evidence packets (policy
  hash moves). Check the `observe_*` cells that exercise the audit still
  carry the same tool sequence, not just the same hash.
- Paired smoke run against the last smoke job in `harness/ledger.jsonl`.
  Threshold, written to `harness/NOTES.md` before the run: kept if
  polyglot-rust-c passes and no task that passed before now fails; at
  k=1 a single polyglot pass is weak evidence, so also count file-tool
  calls and mean tool calls per trial: the prompt must not add more
  than a couple of calls per trial on average.

## Commit 2: a repeated-call cutoff (steering)

### Observed

Three dev trials cost more than $8 each and together $31.45 of the
$103.81 run: `db-wal-recovery#3` 194 calls, $11.59, failed, the same
Python probe issued nine times; `make-mips-interpreter#3` 166 calls,
$11.04, passed; `make-mips-interpreter#1` 119 calls, $8.82, failed. Mean
input per trial 2.0M tokens for passes, 3.5M for failures. The digest
already reports "repeated identical calls" per trial
(`crates/rigcoder-bench/src/digest.rs` ~704). Nothing in the agent acts
on it; the only ceiling is `--max-turns 200`.

### Where

`crates/rigcoder/src/steer.rs`. `BusSet::Gate` sees a tool call before
dispatch (`gate_bash` ~211 answers deny-list matches with `Denied` and a
reason the model reads); `BusSet::Judge` shapes results
(`shape_results`); `RigSet::Judge` turns a missing deliverable into a
`Retry` with feedback (`demand_deliverables`). Rules live in the `Steer`
resource (~36: `deny`, `hold`), compiled in `compile` (~174). Tests in
`crates/rigcoder/tests/steer.rs` use a scripted model
(`run_scripted`, ~103) and pin the deny path
(`a_denied_bash_command_reaches_the_model_as_a_denial_and_never_runs`, ~157).

### Required change

A third rule in `Steer`, `repeat_limit: Option<u32>` (default `Some(3)`),
enforced by a `BusSet::Gate` system next to `gate_bash`: when the same
tool name and byte-identical arguments have already been dispatched
`repeat_limit` times in this run, the call is answered `Denied` with a
reason the model reads, naming the count and telling it to change the
command or inspect its earlier result. Identity is the tool name plus
the exact argument JSON; do not normalise whitespace or paths, a near
duplicate is the model's business. The counter lives in a run-scoped
resource or component, not in `Compiled`, and resets per run. A denied
repeat is a `Denied` disposition like the deny list, so the transcript,
the effect log and the digest see it without new plumbing; check
`shape_results` and the transcript writer treat it as they do a deny.
`validate` rejects `Some(0)`.

Do not lower `--max-turns`; that trades passes on the long tasks that
did succeed at 117 and 166 calls. The cutoff only removes the
byte-identical loop.

### Evidence

- Tests in `crates/rigcoder/tests/steer.rs`: the fourth identical call
  is denied and the fifth, with different arguments, runs; two runs in
  one app keep separate counters; a `None` limit never denies; the
  denial reaches the model as text and the record as `Denied`.
- `rigcoder-bench replay harness/runs/dev-tier-run-dev-1789258759508791000-59132/db-wal-recovery__3`
  cannot vet this (policy hash moves), so instead write a unit test that
  feeds that trial's recorded call sequence (`agent/effects.json`) to
  the counter and asserts the first denial lands on the fourth
  repetition of the Python probe.
- Paired smoke run. Threshold, written before the run: no task that
  passed before fails; mean tool calls per trial not up; at least one
  fewer "repeated identical calls" in the digest if any trial repeats.
  The cost effect shows on the dev tier, which is out of budget; say so
  in the notes rather than claiming a saving.

## Commit 3: consume the Rig fix (after `harness/RIG_FIX_PROMPT.md` merges)

Skip this commit if that PR is not merged; leave a note in
`harness/UPSTREAM.md` with its URL and status.

- Move the pin by the procedure in `harness/ITERATE_PROMPT.md` §4.
- In `crates/rigcoder/src/session.rs`, delete the whole-prompt provider
  retry (the block at ~500 guarded by "A whole-prompt retry is safe only
  before this request has produced tool calls", `resubmit_when_due`,
  `provider_retries`, `retry_at`, and the `transient` heuristics that
  only existed because Rig's `retryable` flag was not trusted for text
  matches). Set the run-level budget Rig now exposes from the same
  setting the session used (`provider_retries`), so the CLI flag and the
  bench manifest keep their meaning.
- Keep the session retry tests (`retry_keeps_both_recorded_attempts...`,
  `rejected_retry_reports_its_actual_failure_once`,
  `each_new_user_request_gets_its_own_retry_budget`,
  `a_backoff_is_busy_and_can_be_canceled...`) as consumer pins of the
  new behaviour, rewritten to drive the run instead of the session; add
  one for a 503 after tool work that now completes without re-running
  the tools.
- Regenerate the `observe_wire/retry_*` and `observe_failures/*_then_answered`
  evidence cells and read their diffs: the retry facts should now come
  from Rig's witness, and `rigcoder/provider_retry` either maps onto them
  or is removed, with the digest updated to read the new fact.
- Paired smoke run; then, if budget allows, the two dev tasks that
  failed on provider errors at k=3 with `-i fix-code-vulnerability -i password-recovery`.

## What to keep updated

`harness/NOTES.md` gets one entry per commit: threshold before, paired
result after, cost per resolved task. `harness/UPSTREAM.md` gets the
Rig PR status. Commit messages state what changed, the failure mode
targeted, and the before/after per-task counts and cost, as
`ITERATE_PROMPT.md` §3 step 7 requires.

## Stop and ask

- The repeat cutoff denies a call in a trial that passed before.
- The prompt change costs a task that passed in all three dev attempts.
- Spend would exceed what remains under the $200 cap.
