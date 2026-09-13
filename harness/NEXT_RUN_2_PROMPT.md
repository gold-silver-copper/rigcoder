# Task: continue the rigcoder iteration loop (run 3)

The rules are `harness/ITERATE_PROMPT.md`; read it first, then this
file, then `harness/NOTES.md` from "Next run, step 1" to the end.

## Where things stand (2026-09-13, end of run 2)

- Rig pin: Rig `main` at the #2504 squash (`a6897db6`); `harness/UPSTREAM.md`
  has the history, no open Rig PRs. If NOTES' last pin entry says the
  checks after that move found drift, start by reading it.
- The harness bucket is empty. Smoke has been 10/10 on the last three
  runs with no `harness` or `rig` event; dev2 measured the fixes at
  54/60 (pass@1 0.90) against 50/60, $1.89 per resolved task against
  $2.08; the one harness mode dev2 found (a turn holding only a thought
  signature settling on an empty answer) is fixed in rigcoder and Rig,
  paired 10/10.
- What fails now is `model`: configure-git-webserver 0/3 on three
  runs (the push-triggered deploy leaves 404), one db-wal-recovery
  spiral in three, one make-mips-interpreter render timeout in three.
- What costs now is context, not calls. On dev2, 86% of input tokens
  were cache reads; the median call carries 26k input tokens, a long
  trial's calls carry 75k to 130k, and the three trials over $9 were
  the ones whose calls grew past 90k tokens each. The four spiral
  signals measured in run 2 do not separate spirals from long passes
  (NOTES has the table); do not revisit them.
- Budget: $282.98 of $400 spent. A smoke run is $9 to $18, a dev run
  about $100. Plan on smoke runs only unless step 2 earns a dev run;
  ask before spending past $370.

## What this run is for, in order

### 1. Confirm the loop is still clean on the current pin

One smoke run on the current binary (`harness/build-linux.sh`; the
manifest's `source_head` must be the commit you are on). Threshold,
written to NOTES before launch: 10/10 or one flip on a `model` cause,
no `harness` or `rig` event, cost per resolved task within the last
three runs' range ($1.12 to $1.76). Classify every failure as always.
If a `harness` or `rig` mode appears, it is the target and the rest
of this file waits.

### 2. The cost lever the data supports: context size per call

The cost tail is calls whose input grows with history. Two candidate
levers, both in `crates/rigcoder/src/steer.rs` and both typed:

- **Result shaping is already there** (`Steer.max_result_chars`,
  default 30 000, `BusSet::Judge`). Measure first: over the dev2
  transcripts, for each tool result, its length and how many later
  calls it stays in history for; compute the input tokens a lower bound
  (10 000, 15 000, 20 000) would have removed per trial, and whether any
  passing trial's later calls read something only the cut part held
  (search the trial's later tool arguments and the final answer for
  strings that appear only beyond the bound). Write the table to NOTES.
- **Head-and-tail for stale results.** A tool result that is older than
  N turns and longer than M chars is replaced in history by its head,
  tail and a one-line marker ("[n chars elided; call the tool again to
  see them]"), while the record keeps the full output. This is a
  `RigSet::Assemble`-time rewrite of the utterance the fold reads, not
  of the record, so replay is unaffected. Measure the same way before
  building: tokens removed per trial at N = 8, 12, 16 and M = 4 000,
  8 000, and the same "did a later call need it" check.

Pick whichever removes more tokens on the spiral trials with zero
hits on the needed-later check across all dev2 passes. If both have
hits, stop and report the table. Build the winner with unit tests
(the rewrite is pure: history in, history out), a witness fact
(`rigcoder/history_trimmed`, chars and turn distance), and the digest
counting it. Evidence packets regenerate (steer.rs is in the policy
hash). Paired smoke, threshold before launch: no passing task lost,
mean input tokens per trial down, a `history_trimmed` fact on at least
one trial. If smoke shows the fact and no loss, one dev run paired
against dev2 for the cost claim; the claim is cost per resolved task
down with pass@1 within the interval.

### 3. configure-git-webserver, a prompt-lane experiment, not a fix

Nine of nine trials fail the same way: the agent configures sshd,
nginx and a hook and settles without running the user's own flow
(`git clone user@server:/git/server`, push, `curl :8080/hello.html`).
The deliverables audit fixed polyglot by naming the check; this is the
same shape. One sentence in `prompt.md`'s audit, in its voice: when the
instruction shows a command sequence, run that sequence yourself
before settling. It is a `model` mode, so it is not a target under the
rules; run it only if steps 1 and 2 are done with budget left, as its
own commit with its own paired smoke (the prompt is not in the cells'
identity, so only stream batch numbers move), and judge it on
configure-git-webserver at k=3 (`-i configure-git-webserver -k 3`,
about $3) rather than on smoke.

### 4. Propose the holdout

If step 1 is clean and step 2 lands, the loop has converged twice on
smoke and been measured twice on dev. Propose the holdout run to the
user with the exact command and cost; never run it, never read its
per-task results.

## Learned in run 2, added to the rules

- A command that can exceed ten minutes (a full dev run, the Rig CI
  selection, the evidence regeneration plus workspace tests) is
  started with `nohup ... ; echo done > <marker>` and waited on with an
  `until [ -f marker ]` loop; a plain background call is killed at the
  tool's cap and nextest reports the survivors as SIGTERM failures.
- Twenty images building at once can exceed a task's 600 s build
  timeout and abort the run before any trial; the second launch finds
  them cached. No spend is lost.
- A matrix cell for a behaviour a live provider cannot be made to show
  (an empty answer on cue) is pinned with a scripted model in
  `tests/steer.rs`, not with a derived two-exchange cassette: the
  second request's body changes and cannot be edited by hand. Cells
  whose recorded answer already has that shape run with the rule off
  (`steer: Some(|s| s.max_empty_retries = 0)`).
- Rig `main` moves between a PR's branch head and its squash; after a
  merge, diff `main` since the previous pin and expect cassettes whose
  second request carries assistant history to need re-recording when
  message rendering changed (#2501 did that).
- Never glob `harness/runs/*` for measurements; exclude `holdout-*`
  explicitly. Run 2 read holdout per-task results once by mistake and
  disclosed it in NOTES.

## Stop and ask

- A `harness` or `rig` mode survives two targeted fixes.
- Both context levers have a needed-later hit on a passing trial.
- Spend would pass $370.
- Before the holdout, always.

Report: the smoke result, the context-lever table with the choice and
its paired results, configure-git-webserver at k=3 if run, commits with
per-task deltas and cost per resolved task, spend, the current pin.
