# Iteration notes

Terse log of the eval → diagnose → fix → re-eval loop described in
`ITERATE_PROMPT.md`. One entry per iteration.

## Setup (2026-09-12)

- Rig pin: `896bb8b4c62a21df9bb97a5973216c41ed995001` (Rig main after
  #2443 and #2498).
- Model: `gemini/gemini-3.8-flash`. The trusted gateway
  (`--gemini-budget`) only admits host-scored tasks (exact output,
  Polyglot, Vim); every smoke/dev task is scored by its in-container
  `test.sh`, so runs pass `GEMINI_API_KEY` directly and spend is computed
  from the bench ledger's token counts at `gemini_budget.py` rates:
  $0.75/M input, $3.75/M output.
- Budget: the ledger's own limits. Development phase $110, total $198
  under the $200 hard cap (raised from $20 on 2026-09-12). Historic easy trials cost $0.12–0.27 each.
- Smoke slice: `slices/smoke.txt`, 10 dev tasks, `-k 1`, concurrency 4.
- Acceptance threshold, set before any run: a change is kept only if it
  removes the targeted `harness`/`rig` failure mode on the paired
  per-task comparison and adds no new `harness` failure. With 10 tasks
  at `-k 1`, a ±1–2 task delta in pass count is noise and decides nothing.
  Cost per resolved task is reported next to pass count every time.
