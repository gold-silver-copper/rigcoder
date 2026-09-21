Improve this coding harness using evidence from existing runs. Own the investigation and make the engineering recommendation; do not ask me to choose an architecture.

Read AGENTS.md, harness/ITERATE_PROMPT.md, harness/NOTES.md, and the current repository state first. Preserve unrelated changes. Do not create a PR, access holdout task data, or make paid model calls during this task.

Context:
- The retry-reporting fix is retained and verified.
- Paired smoke results were 10/10 → 9/10, with cost per resolved task $0.92 → $2.60. The failing db-wal-recovery run was classified as a model failure.
- The proposed turn-folding design failed its predeclared gate. Do not implement it or weaken that gate retrospectively.
- Bounded checkpoints with transcript retrieval are an untested idea, not the chosen solution.
- Recorded cumulative spend is approximately $333.97 of the $700 cap; verify the ledger before relying on this figure.

Your task is to audit existing development traces and recommend one concrete experiment.

1. Establish the evidence.
Inventory available development runs, their evaluated commits, configurations, outcomes, and recorded costs. Distinguish comparable runs from those with different settings. Do not infer a regression or improvement from one noisy pair.

2. Investigate failures and expensive successes.
Audit all failed development trials and the highest-cost successful trials. Identify:
- Where progress stopped or unnecessary work began.
- What information was available to the model at that point.
- Whether the cause was model reasoning, tool behavior, execution infrastructure, history handling, or another harness mechanism.
- Whether a specific runtime intervention could plausibly help, and how it could harm successful runs.

Support findings with concrete trace references, tool calls, and outcomes. Separate observed facts from hypotheses. Do not treat repeated calls or word overlap alone as proof of a problem.

3. Rank opportunities.
Group recurring mechanisms. Quantify affected trials and their share of recorded cost where possible. Rank opportunities by evidence strength, likely benefit, implementation complexity, and regression risk. Do not invent estimated savings when the traces cannot support them.

4. Select one experiment.
Choose the smallest runtime change supported by the audit. Do not use task-specific rules, answer leakage, or prompt tuning against individual failures. If no change has enough evidence, recommend the smallest additional measurement needed instead.

Specify:
- The causal hypothesis and concrete before/after behavior.
- Implementation scope and relevant code locations.
- Offline checks that could reject the idea before paid evaluation.
- A controlled baseline/candidate evaluation using existing benchmark tooling, repeated development trials, and fixed settings.
- Predeclared success, rejection, and stopping criteria, prioritizing tasks solved and then cost per resolved task.
- Accounting for all added calls, retries, checkpointing, or retrieval costs.
- The proposed evaluation budget.

Deliver a concise audit artifact in the repository with trace references and one recommended experiment. Lightweight offline analysis scripts are allowed; do not build a new benchmark framework or implement the candidate runtime change yet. Run appropriate checks for any files changed.

Finish by explaining what we should do next, why the evidence supports it, what remains uncertain, and the exact scope and budget of the proposed next step.
