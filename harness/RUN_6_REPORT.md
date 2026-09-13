# Run 6 report — 2026-09-13

Executes `NEXT_RUN_6_PROMPT.md` on the pin `234626eb` (commit `b86457d`). Budget $20; spend is tallied at the end. No holdout access, no task/verifier/timeout/image change; the benchmark `task.toml` files are untouched.

## Step 1 — Harbor egress-controlled runner

**What Harbor 0.22.0 enforces.** Every task container that does not declare its own networking is started with `network_mode: service:harbor-docker-egress-control-sidecar`, i.e. it shares the sidecar's network namespace. The sidecar (`environments/docker/harbor-docker-egress-control-sidecar/`) runs `gost` as a transparent proxy with a file-backed whitelist and an nftables ruleset that redirects every outbound TCP connection to the proxy and rejects every non-TCP, non-local, non-ICMP packet (`bin/network-policy`). Modes: `public` (ruleset removed), `no-network` (empty whitelist), `allowlist` (hostnames, wildcard hostnames, IPs, CIDRs). The mode is declared per phase in **task.toml** (`[environment] network_mode` baseline, `[agent]` and `[verifier]` overrides); the job CLI can only *add* hosts (`--allow-agent-host`, `--allow-environment-host`), and warns that they are ignored when the effective mode is public (`trial/network_policy.py:24-40`). The policy is switched with `network-policy` in the sidecar before and after `agent.run()` and before `verify()` (`trial/trial.py:262-277`).

Consequences: by construction a shell or Python process in the agent phase cannot complete an HTTP exchange with a host outside the allowlist, whether by name, direct IP, or redirect — the TCP connect succeeds (it lands on the proxy) and the proxy then closes the connection without a response. DNS itself is UDP and is rejected by the filter chain. A verifier phase without its own override inherits the environment baseline (public by default), so unchanged `test.sh` installs keep working. **None of the twenty benchmark tasks declares a network mode, so Harbor runs them public unless a task.toml is edited**; the only way to use the boundary without editing benchmark files is a job-owned snapshot copy with the policy added, which is what the trial below used.

**Adapter.** `harness/rigcoder_agent.py` restored (commit `75f7b1a`): `--task-file`, `--effect-log`, `--observations`, `--transcript` into the trial's agent dir; binary chosen by the container's `uname -m` (Harbor uses the task's prebuilt amd64 image under emulation unless `--force-build`; the first paid attempt failed on that with exit 127, $0); `RIGCODER_NETWORK_PROBE=1` records an HTTP probe from inside the agent phase.

**Offline check.** `PYTHONPATH=. harbor run -p harness/tasks/cancel-async-tasks -a harness.rigcoder_agent:RigcoderAgent --install-only` — 1 trial, 0 exceptions, upload and CA-store command ran (`harness/runs/harbor-install-check`). The repo root must be on `PYTHONPATH` for the import path.

**Paid trial** (`harness/runs/harbor-egress-trial-2`, scratch copy of cancel-async-tasks with `[agent] network_mode="allowlist"`, `allowed_hosts=["generativelanguage.googleapis.com"]`, `--force-build`): reward **1.0**, 57 tool calls, settled; 1,650,385 input / 17,146 output / 1,319,922 cached tokens = **$1.3021** at the repository's rates; transcript, `effects.json`, `observations.json` in the trial's agent dir; the probe from inside the agent phase: `http://93.184.215.14/` DENIED (RemoteDisconnected), `https://example.com/` DENIED (SSL EOF), `http://example.com/` DENIED — all three closed by the proxy with no response; the unchanged verifier installed uv/pytest in the public verifier phase and passed 6/6. Recorded in `ledger.jsonl` as slice `custom`, meta_commit `harbor-0.22.0`.

Not proven: a bash tool call from the *model* trying to fetch answers (the probe used the same exec path but was not a model action); Harbor's token accounting for rigcoder relies on the adapter's transcript sums; no leaderboard-comparable full run.

## Step 2 — contamination column (commit `4cf0cae`)

`digest::benchmark_reference` matches tool-call arguments against `laude-institute/terminal-bench`, `terminal-bench-2`, `solution/solve.sh` (case-insensitive). `TrialFacts.contamination` lists `(transcript line, reference)`; `digest` renders a "Contaminated trials" section and drops them from both buckets. `TrialRecord.contaminated` is set from the transcript at trial end; `stats::summarize` excludes such trials from score, pass@1, pass@k and the Wilson interval, records `contaminated` in the summary (hence in the ledger and the keep/revert comparison) and keeps their cost; their reward stays as evidence, never recoded as a failure. Tests: `benchmark_references_mark_a_trial_contaminated_but_not_its_reward` (digest) and `contaminated_trials_are_neither_passes_nor_failures_but_still_cost` (stats).

Over the 21 explicit non-holdout jobs: **2 contaminated trials** — `c4-walk…/db-wal-recovery__1` (first reference `terminal-bench-2`, L263) and `gen-001…/db-wal-recovery__U8r8faT` (L254). A first draft that also matched bare `terminal-bench/` flagged nine clean constraints-scheduling passes through the task's own calendar `PRODID:-//Terminal-Bench//`; that pattern was removed. Lower bound only: encoded or indirect fetches are not detected.

## Step 4 — MIPS graphics-init stdout mismatch ($0, reproduced)

Reconstructed `/app/vm.js` for both failing trials (single `write_file`, no later edits) and two passing ones, rebuilt the task image (its build-time `doom.wad` is the 341-byte ibiblio error page; the shareware `doom1.wad` the passing trials fetched was used), and ran the verifier's exact spawn/terminate sequence (`tests/test_outputs.py:10-58` mirrored, no assertion).

- Valid WAD, fresh `/tmp`: `I_InitGraphics…` present for all four `vm.js`, frame after 1.5 s. Node buffering is not the cause (`process.stdout.write`, `fs.writeSync(1)`, `console.log` all survive SIGTERM on Node 18 and 22).
- Valid WAD, **stale `/tmp/frame.bmp` left by the agent's own probes**: the verifier sees the frame at once, sleeps 1 s, terminates. At full CPU the text is already out (FOUND); at `--cpus 0.5` or `0.25` it is not (**MISSING**, 540 bytes of stdout) — for the failing *and* the passing `vm.js`. Without the stale frame the same starved run prints it (FOUND, frame after 5.8 s).
- Both failing trials ended with a frame in `/tmp` (dev-tier L224 listing; dev2 L259 `rm … && node … && ls` recreates it), and both ran in a k=3, n=4 job.

Conclusion: a verifier race (it never clears `/tmp/frame.bmp`, and its 1 s grace is shorter than the interpreter's start-up under load), triggered when the agent leaves its probe frame behind and the host is loaded by concurrent trials. Not a rigcoder process-handling bug; not fixable without touching the verifier (prohibited). Recorded as verifier/infra flakiness, not model. A generic mitigation that stays inside the harness is lower concurrency for this task; the agent-side habit of removing probe artifacts is task-specific and not proposed as a prompt rule.

## Step 3 — db-wal-recovery input preservation

Threshold stated in `NOTES.md` before running. Checkpointed k=1 run (`r6-dbwal-ckpt…`): **pass**, 30 calls, $0.227; this attempt backed up both files at call 3 before any open, so it does not itself exhibit the failure. Branch point: turn 2 (after `ls -la`, before the backup decision).

`replay --prompt-file` cannot evaluate a prompt edit under the current pin: the effect log's policy hash covers the prompt, so a changed prompt is refused outright (`replay refused: the log was recorded under policy …`); trials recorded before the pin move are refused even with the unchanged prompt. The threshold's replay condition is therefore unprovable, not failed.

**Baseline branch** (current prompt, `r6-dbwal-branch-baseline…/db-wal-recovery__branch2-1`): from the branched turn the agent's first call was `sqlite3 /app/main.db "SELECT * FROM sqlite_master"`, the second another `SELECT`, and by call 3 `/app/main.db-wal` no longer existed — the recorded failure mode, reproduced at the branch point. It then entered the recovery spiral (`git status`, `find / -name "*main.db*"`, …). Stopped by hand at 128 calls to protect the budget; the salvaged transcript is in the trial's agent dir with `STOPPED.txt`. Its usage is **unknown** (rigcoder writes usage at settlement); reserved at $8 from comparable failed trials. Attempts 2–3 of the baseline were not run.

**With the rule** (prompt.md: "Preserve task inputs before touching them…", `r6-dbwal-branch-rule…`, `--max-turns 40`): 3/3 attempts' **first call** copied both `main.db` and `main.db-wal` to `/tmp`; 3/3 passed the verifier in 25, 31 and 24 calls; $0.176 + $0.290 + $0.191 = **$0.657**. Threshold met (all preserve before opening, ≥2 pass); the replay condition is unprovable (above). **Kept**, commit below. `cargo test -p rigcoder --test gemini_observe` still passes (71) with the new prompt; no fixture regeneration was needed.

Caveats: one branch point from one passing trial; the branched turn already included the instruction and `ls -la`. Not evidence for other tasks; the rule is generic wording, not a WAL or SQL rule. Cost per attempt with the rule ($0.18–0.29) versus the $11–12 recorded failures is the case for it, on this task only.

## Step 5 — smoke (run after the user raised the cap to $200)

`r6-smoke-run-smoke-1789338864563970000-68220`, new binary (pin `234626eb`, prompt rule), 10 tasks, k=1, n=4, paired against `r4-smoke-run-smoke-1789329109644530000-79506`:

| task | r4 | r6 | r4 $ | r6 $ | calls r4/r6 |
|---|---|---|---:|---:|---|
| cancel-async-tasks | 1 | 1 | 1.697 | 1.955 | 72/73 |
| chess-best-move | 1 | 1 | 0.413 | 1.168 | 35/60 |
| db-wal-recovery | 1 | 1 | 0.129 | 0.181 | 22/27 |
| extract-elf | 1 | 1 | 2.014 | 1.257 | 55/51 |
| git-leak-recovery | 1 | 1 | 0.155 | 0.413 | 30/47 |
| git-multibranch | 1 | 1 | 0.899 | 1.240 | 66/69 |
| headless-terminal | 1 | 1 | 2.137 | 2.262 | 92/93 |
| kv-store-grpc | 1 | 1 | 0.623 | 0.633 | 35/34 |
| password-recovery | 1 | 1 | 0.484 | 0.353 | 31/30 |
| polyglot-rust-c | 1 | 1 | 0.680 | 0.965 | 45/60 |

**10/10 → 10/10**; taxonomy: infra 0, harness 0, rig 0, model 0; contaminated 0; every trial settled with known usage. Cost $9.231 → **$10.428** ($0.923 → $1.043 per resolved trial, +13%), inside the recent smoke range ($8.98–$23.44) and within k=1 noise; no single task shows a systematic cost shift attributable to the rule (chess-best-move and git-leak-recovery went up, extract-elf and password-recovery went down). Both jobs pass db-wal-recovery; the rule's target failure was seen in `r4-retry-reason`, not in this baseline, so the smoke pairing is a no-regression check, not a demonstration of the fix. The kept-change rule holds: no harness/rig failures added, no task lost.

## Spend

| Item | Cost |
|---|---:|
| Harbor trial 1 (agent failed to start, exit 127) | $0 (no model call) |
| Harbor trial 2, cancel-async-tasks-egress, pass | $1.302 |
| r6-dbwal-ckpt, k=1 checkpointed, pass | $0.227 |
| r6-dbwal-branch-baseline attempt 1, stopped at 128 calls | **unknown**, reserved $8 |
| r6-dbwal-branch-rule, 3 attempts, 3 passes | $0.657 |
| MIPS reproduction, digest, adapter offline check | $0 |
| r6-smoke, 10 tasks k=1, 10/10 | $10.428 |
| **Known total / with reserve** | **$12.614 / ≤ $20.61** (cap raised to $200 by the user before the smoke) |

Rates: $0.75/M input, $3.75/M output, cached input included. Ledger: `r6-dbwal-ckpt` and the Harbor job are recorded; the branch jobs are not ledger rows (branch-from does not write the ledger); their costs are in this table and the trial transcripts.

## Commits

- `b86457d` pin move (previous session, same day) — precondition.
- `4cf0cae` feat(bench): contamination column.
- `75f7b1a` feat(harness): Harbor adapter with arch-aware install and egress probe.
- `f0296bf` feat(prompt): preserve task inputs before the first mutating command.

## Passed / failed / unproven

Passed: Harbor install-only; Harbor egress trial (probe denied, reward 1.0); digest tests (92) and the two known contamination cases with zero false positives on the other 19 jobs; MIPS reproduction under CPU starvation; with-rule branch 3/3 preserve + 3/3 pass; section-0 checks after each commit.
Failed: Harbor trial 1 (arch mismatch, fixed in the adapter); the first contamination pattern set (nine false positives, fixed).
Unproven: replay of a prompt edit (policy-hash refusal); baseline branch cost (unknown, reserved); the rule's effect on any task other than db-wal-recovery; a smoke pairing on the new binary; Harbor token accounting beyond the adapter's transcript sums.
