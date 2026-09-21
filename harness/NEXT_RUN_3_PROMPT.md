# Task: continue the rigcoder iteration loop (run 4)

The rules are `harness/ITERATE_PROMPT.md`; read it first, then this
file, then `harness/NOTES.md` from "Run 3, step 1" to the end.

## Where things stand (2026-09-13, end of run 3)

- Rig pin: Rig `main` at the #2510 squash (`387abeea`);
  `harness/UPSTREAM.md` has the history, no open Rig PRs. The move
  carried one semantic change: a Gemini prompt block is a typed
  refusal on the report (`kind: provider_response`, `refusal: true`,
  the block reason as `code`, never retryable). `FailureDetail` carries
  `refusal`; the digest keys refusals off the witness action and did
  not move. No smoke has run on this pin yet.
- The harness bucket is empty. Smoke has been 10/10 on the last four
  runs with no `harness` or `rig` event; dev2 measured the fixes at
  54/60 (pass@1 0.90) against 50/60, $1.89 per resolved task against
  $2.08.
- What fails now is `model`: configure-git-webserver 0/3 on four runs
  (the agent now runs the user's clone, push and curl itself, sees
  `403`, and settles anyway), one db-wal-recovery spiral in three, one
  make-mips-interpreter render timeout in three. Run 3 showed that a
  prompt sentence changes what the agent does and not whether it
  believes what it sees; do not spend on prompt-lane experiments
  against `model` modes again unless the change is a mechanism (a
  check the runtime performs), not an instruction.
- What costs now is history, not calls and not single results. Run 3
  measured both bounded-result levers over dev2 (NOTES has the table):
  the best case removes a tenth of the input and every setting has
  needed-later hits on passing trials. The cost is the history growing
  call by call. A lever on that changes what the model knows and is a
  design decision; this run makes it, under a rule set before building.
- Budget: $301.30 of $700 spent (the cap was raised from $400 after
  run 3). A smoke run is $9 to $18, a dev run about $100. This run's
  plan is two smoke runs and, if step 2's smoke passes, the paired dev
  run; ask before spending past $670.

## What this run is for, in order

### 1. Confirm the loop is still clean on the new pin

One smoke run on the current binary (`harness/build-linux.sh`; the
manifest's `source_head` must be the commit you are on). Threshold,
written to NOTES before launch: 10/10 or one flip on a `model` cause,
no `harness` or `rig` event, cost per resolved task within the last
three runs' range ($1.12 to $1.76). Classify every failure as always.
Any trial whose failure record carries `refusal: true` is a new row in
the taxonomy (the pin made it visible for the first time); say what
was refused and whether the deny list or the model asked for it.
If a `harness` or `rig` mode appears, it is the target and the rest
of this file waits.

### 2. The history lever, as a design with a rule

The design is *turn folding*: a completed tool exchange older than N
model calls is replaced in the history the fold reads by a compact
form, while the record keeps the full exchange, so replay and the
witness are unaffected. It is a `RigSet::Assemble`-time rewrite of the
utterances, pure (history in, history out), in
`crates/rigcoder/src/steer.rs` beside result shaping, and typed
(`Steer.fold_after_calls: Option<usize>`, default off until measured).

The compact form keeps what the run-3 hits said later calls reach
for, which was names and paths more than content: the tool name, its
arguments, the result's first and last lines, its exit status if any,
and a one-line marker with the elided size and "call the tool again to
see it". A tool result that a later call's arguments quote verbatim
(a path, a symbol) stays unfolded for that call's turn; this is the
needed-later check from run 3 made into the rule itself rather than a
veto on it.

Measure before building, over the dev2 transcripts and excluding
`holdout-*`: tokens removed per trial at N = 6, 10, 16; the hits
count under the run-3 definition; and the hits count with the
quote-check applied (a hit that the quote-check would have kept
unfolded is not a hit). Write the table to NOTES. The rule for
building: at some N, removal on the long trials is at least a fifth of
their input and the quote-checked hits on passing trials are at most
two, each explained by reading the trial. If no N meets it, stop and
report the table; the design is then not this one.

Build the winner with unit tests on the rewrite, a witness fact
(`rigcoder/history_folded`: turns folded, chars elided, N) and the
digest counting it. Evidence packets regenerate (steer.rs is in the
policy hash); expect only batch numbers to move on cells shorter than
N calls, and say which cells fold. Paired smoke against step 1,
threshold before launch: no passing task lost, no `harness` or `rig`
event, mean input tokens per trial down, a `history_folded` fact on at
least one trial, and no trial with more tool calls than its pair by
more than the folded count (re-fetches are the lever's price; count
them). If smoke passes, run the dev run paired against dev2, threshold
before launch: cost per resolved task down and pass@1 within dev2's
interval; a lost task that a re-fetch could have saved (the folded
result's content appears in the failing trial's later arguments) is a
`harness` mode and the lever's setting is the target.

### 3. Rig follow-ups the pin surfaced, if any

The #2510 migration deferred two things that are Rig's, not
rigcoder's: the gRPC Gemini wire does not read `prompt_feedback`, so a
block there is neither an error nor a refusal; and the two Venice
termination tool-turn cells are ignored until someone records their
cassettes. Neither is on rigcoder's wire. Open nothing for them; note
in NOTES if step 1 shows a refusal that the report did not type.

### 4. Propose the holdout

If step 1 is clean and step 2's dev run meets its threshold, propose
the holdout to the user with the exact command and cost. Never run
it, never read its per-task results.

## Learned in run 3, added to the rules

- `cargo nextest` runs each test in its own process, so the observe
  suite's in-process one-cell-at-a-time lock does not hold across
  processes and the lineage cells that share one workspace race. CI's
  `cargo test` is one process and is the verdict; under nextest, run
  the observe suite with `-j1`.
- rigcoder-verify's sandboxed compile test has a 30 s limit and can
  miss it while the rest of the workspace compiles beside it; rerun it
  alone before calling it a failure.
- Two serial regenerations of the evidence packets differ in batch
  numbers and in the dispatch order among a batch's concurrent calls
  (`order`, `effect`, `id`); those are measurements and never a
  finding. The facts as a set, and every `cell.json`, must not move.
- When a Rig PR is merged somewhere other than `main`, a merged PR
  cannot be reopened; cherry-pick its squash onto `main` as a new PR
  and say so in its body. Audit "did it reach main" by base branch and
  by content (cherry-pick onto a detached `origin/main` and inspect the
  index), never by merge-commit ancestry alone: squashes and rewritten
  branches make ancestry lie in both directions.
- zsh does not word-split unquoted variables; loops over "a b" pairs
  need `${=pair}` or `${pair%% *}`/`${pair##* }`.
- A one-sentence prompt change against a `model` mode is not a fix
  and, after run 3, not an experiment worth a run either.

## Stop and ask

- A `harness` or `rig` mode survives two targeted fixes.
- No N meets the rule in step 2.
- Spend would pass $670.
- Before the holdout, always.

Report: the smoke result and any refusal rows, the folding table with
the N chosen and its paired results, commits with per-task deltas and
cost per resolved task, spend, the current pin.
