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

## Iteration 0: baseline (2026-09-12)

- Job `baseline-smoke-run-smoke-1789254820590005000-25076`, commit `f3fed29`.
- Smoke 10/10 passed. Cost $10.43, $1.04 per resolved task. Input tokens
  13.5M (1.35M mean per trial, cache 10.3M), output 88k.
- Taxonomy of failed trials: none. Non-fatal harness events in passing
  trials: 3 steering denials. Two were the deny list doing its job
  (`ls -la /`, `find / -name ...`). One was `write_file` refusing a `.rs`
  file with "rustfmt is required to prepare Rust edits" in a container
  without a Rust toolchain (polyglot-rust-c); the model recovered with a
  bash heredoc. Classified `harness`: the edit path must not depend on
  the container's toolchain.
- Observation, not a fix target: bash carries 49.9 of 53 mean tool
  calls; edit_file 0.1. Mean 41.7 calls before the first edit.
- Fix for iteration 1: a missing rustfmt writes the file as authored
  and says so in the receipt; formatter failures still refuse.
  `rigcoder-bench replay` cannot vet this: any tool-source change moves
  the policy hash and replay refuses the log.

## Iteration 1: missing rustfmt no longer refuses Rust writes (2026-09-12)

- Job `iter1-rustfmt-run-smoke-1789256018318594000-65994`, paired
  against iteration 0 on the same slice, model and settings.
- Threshold (set in iteration 0): keep if the rustfmt denial is gone
  with no new `harness` failure; ±1–2 tasks is noise.
- Paired per task: 9/10 vs 10/10. The one flip is polyglot-rust-c,
  bucket `model`: the model wrote everything through bash heredocs
  (zero write_file/edit_file calls), so the fix was not exercised, and
  it left `main` and `cmain` binaries next to `main.rs`, which the
  verifier rejects. No denial occurred.
- Denials: 3 → 1; the remaining one is the deny list refusing
  `find /` in kv-store-grpc, as designed. `harness` events: 1 → 0.
- Cost: $9.65 vs $10.43; per resolved task $1.07 vs $1.04 (noise).
  Smoke spend to date $20.08 of the $110 development limit.
- Decision: kept as a clear correctness fix pinned by unit tests; the
  benchmark neither confirmed nor contradicted it.
- Evidence packets regenerated: only the policy hash plus timing-bound
  stream batching changed.
