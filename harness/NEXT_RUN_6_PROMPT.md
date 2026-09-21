Improve rigcoder and its harness in this order. Read AGENTS.md if present, harness/ITERATE_PROMPT.md, harness/DEVELOPMENT_AUDIT.md, and harness/NETWORK_ISOLATION_FEASIBILITY.md first. Inspect current code and repository state before relying on prior conclusions. Preserve unrelated uncommitted changes.

Constraints:
- Budget: $20 for this iteration in provider calls, all through gemini/gemini-3.8-flash, all recorded in harness/ledger.jsonl. Stop and report if a step would exceed it.
- No holdout access. No task, verifier, timeout, or task-image changes. No tool-text filters or broad domain allowlists as enforcement.
- Rig stays pinned at 234626eb8e75ee8af76e364012e3fd358119360b unless a rig-bucket failure forces the section 4 procedure.
- Rebuild harness/bin/rigcoder with harness/build-linux.sh before any paid run; the current binary predates the pin move.
- Section 0 checks green after every commit. Commit each step on its own.

Step 1 — Harbor egress-controlled runner ($0).
Harbor 0.22.0 is installed at ~/.local/bin/harbor. It ships agent-phase egress control (docker-compose-egress-control.yaml, the harbor-docker-egress-control-sidecar, --allow-agent-host / --allow-environment-host). Read how it is enforced and what its default network baseline is; state whether it denies direct-IP, hostname and redirect egress from candidate shell and Python by construction, and whether the verifier phase keeps network so unchanged test.sh installs still work.
Restore the deleted adapter (git show 72e94d5^:harness/rigcoder_agent.py) as harness/rigcoder_agent.py, updated for the current CLI (--task-file, --effect-log, --observations, --transcript) and the current BaseInstalledAgent API (install, run, populate_context_post_run, exec_as_root, logs_dir, model_name, _extra_env). Keep provider key handling through --ae only; never print it.
Verify offline: harbor run -p harness/tasks/cancel-async-tasks -a harness.rigcoder_agent:RigcoderAgent --install-only -o harness/runs. Then run one paid trial of that task through Harbor with the egress baseline (no extra allow hosts) and confirm: reward recorded, transcript and effect log in the trial's agent dir, tokens in the Harbor result, and a probe of the boundary from inside the trial (a bash tool call to curl a synthetic external host must fail). Record the Harbor job in ledger.jsonl with its measured cost. Do not add a second native runner; Harbor is the trustworthy-score path, rigcoder-bench remains the cheap loop.

Step 2 — contamination flag in digest ($0).
Extend rigcoder-bench digest (crates/rigcoder-bench/src/digest.rs; do not write a parallel parser) with a per-trial contamination column: any tool call whose command or arguments reference the public benchmark repository (laude-institute/terminal-bench, its raw.githubusercontent and api.github paths, or a harness/tasks-relative solution/tests path) marks the trial contaminated. Contaminated trials are excluded from the keep/revert comparison and reported separately; they are never recoded as failures. Unit-test it on the two known cases (runs/c4-walk-run-smoke-1789279457499080000-67562/db-wal-recovery__1 and runs/gen-001-1788759391/db-wal-recovery__U8r8faT) and on one clean pass. Re-run digest on the 21 explicit non-holdout jobs and report the contaminated count; it must be at least two.

Step 3 — db-wal-recovery early input loss (≤ $8).
Evidence: dev-tier db-wal-recovery__3 L6–11, dev2 attempt 1 L4–7 and r4-retry-reason attempt 1 L4–7 open the original database before backing up its WAL; the WAL disappears; the trial fails after $11–12. Passing trials back up first. Hypothesis: a generic, task-agnostic rule in crates/rigcoder/src/prompt.md ("before a command that may mutate or consume task inputs, preserve a copy") removes this failure mode. No SQL-, WAL- or task-name-specific wording.
State the acceptance threshold in harness/NOTES.md before running. Test with the cheap path first: none of the failed trials has checkpoints, so run the task once with --checkpoint via rigcoder-bench run -k 1 on db-wal-recovery only (~$3–12), then rigcoder-bench branch-from that trial at the turn before the first mutating open, --times 3, with the prompt change applied (~$1–3). Keep the change only if all branched attempts preserve an input copy and at least two pass; otherwise revert and record why. Check replay on one recorded smoke trial to show the rule does not alter an unrelated trajectory.

Step 4 — MIPS graphics-init stdout mismatch ($0).
dev-tier make-mips-interpreter__1 and dev2 make-mips-interpreter__1 pass frame existence and similarity but fail the expected graphics-init stdout assertion (verifier/output.txt L247–248). Using the recorded trial artifacts only, rebuild the task image, restore the trial's workspace state if a checkpoint or artifact allows it, and run the verifier's own invocation twice: once as recorded, once with stdout unbuffered (stdbuf/PYTHONUNBUFFERED or the interpreter's flush). Report whether the assertion outcome changes. If it does, that is a harness/runtime finding to fix in the agent's process handling, not the verifier; if it does not, classify as model and stop. No verifier edit.

Step 5 — smoke only if Step 3 kept a change (~$10).
Run the smoke slice once with the rebuilt binary, compare paired per-task results and cost per resolved trial against r4-smoke-run-smoke-1789329109644530000-79506, taxonomy first. Do not run dev in this iteration.

Deliver:
- A short report: what Harbor's egress control enforces and the one-trial result; the contamination column and counts; the db-wal branch-from outcome against the pre-stated threshold; the MIPS reproduction outcome; smoke pairing if run.
- Per-step commits with before/after per-task passes and cost where applicable.
- NOTES.md entry, UPSTREAM.md unchanged unless the pin moved.
- Exactly what passed, failed, or remains unproven, and total spend against the $20 budget.
