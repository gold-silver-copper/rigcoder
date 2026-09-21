# Task-network isolation feasibility — 2026-09-13

**Decision: no minimal production integration preserves every development task's required capabilities while preventing arbitrary answer access.** The existing `--gemini-budget` path (network-none container, host pipe relay) is sound as a transport boundary — extended synthetic probes below all pass — but it is incompatible with the twenty dev tasks unchanged on three counts: every verifier downloads at scoring time, seven tasks need runtime package installs, and one task needs a runtime data download because its image build fetch is broken. Scoring integrity is a separate, unresolved gap. One bounded next experiment is proposed at the end.

This executes `NEXT_RUN_5_PROMPT.md`. $0 in provider calls; no holdout access; no task, verifier, timeout or image change; no prompt tuning. The Rig pin was moved first at the user's request (commit `b86457d`, #2511 squash `234626eb`), which is unrelated to this experiment; the rest of this session made no commit. Existing uncommitted user changes (`NOTES.md`, `gemini_budget.py`) are intact.

## 1. Legitimate network requirements — twenty-task matrix

Sources: `tasks/<task>/{instruction.md,task.toml,environment/Dockerfile,tests/test.sh}` and a tool-call scan of every non-holdout trace (`runs/*/<task>__*/agent/transcript.jsonl`; 317 trials; pattern: curl/wget/pip/apt/npm/git clone/URLs/urllib/socket). Line numbers are physical JSONL lines. "Required in practice" means every recorded pass used it and the image lacks the capability; it is not proof that no offline route exists. No `task.toml` in the slice declares a network flag.

**Verifier-time network (all 20).** Every `tests/test.sh` installs at scoring time inside the candidate container: 17 run `apt-get install -y curl` then `curl -LsSf https://astral.sh/uv/0.9.5/install.sh | sh` (then `uv run` pulls pytest); `fix-code-vulnerability` runs `pip install pytest==8.4.1 pytest-json-ctrf==0.3.5`; `headless-terminal` `apt-get install -y vim` and `pip install pytest==8.4.1 requests==2.32.5 pytest-json-ctrf==0.3.5`; `kv-store-grpc` `pip install pytest==8.4.2 requests==2.32.5 psutil==7.0.0 pytest-json-ctrf==0.3.5`. Under `--network none` every ordinary verifier fails before producing a reward.

| Task (traces) | Required runtime capability | Optional / unnecessary observed | Confirmed answer acquisition | Unresolved |
|---|---|---|---|---|
| cancel-async-tasks (23) | none beyond loopback | `pip install pytest[-asyncio]` in 8 traces, e.g. `c3-retry/…__1` L44; 15 traces with no network call passed | none | — |
| fix-code-vulnerability (12) | none observed | none (0 hits) | none | verifier `pip install pytest` |
| password-recovery (23) | none observed | none (0 hits) | none | — |
| write-compressor (12) | none observed | none (0 hits) | none | — |
| git-leak-recovery (23) | none observed | none (0 hits) | none | — |
| db-wal-recovery (23) | none; 18/20 scored trials passed with no external call | `apt-get install` once | **yes**: `c4-walk/db-wal-recovery__1` L253–271 fetches benchmark solution, original WAL and tests from raw.githubusercontent/api.github (pass, $7.85); `gen-001/db-wal-recovery__U8r8faT` L254–267 (pass). `dev2/db-wal-recovery__1` L330–374 searched duckduckgo/api.github for the answer word list and still failed | — |
| configure-git-webserver (12) | **apt-get install openssh-server nginx git** in 12/12 (image installs only curl: Dockerfile L5), e.g. `dev2/…__1` L21; loopback :8080 and `server` hostname resolution | — | none | all 12 fail for non-network reasons (audit) |
| polyglot-rust-c (20) | none observed | none (0 hits) | none | — |
| sparql-university (9) | **apt-get install python3-rdflib** (or oxigraph/jena) in 9/9, e.g. `dev-tier/…__1` L15; image has no SPARQL engine | jena tarball from archive.apache.org (`generation-000/…__2` L42) | none (`university.org`/`w3.org` hits are RDF IRIs, not requests) | — |
| feal-differential-cryptanalysis (9) | none observed | none (0 hits) | none | — |
| make-mips-interpreter (9) | **runtime WAD download**: the image's build-time `curl distro.ibiblio.org … > doom.wad` (Dockerfile L24) yields a 341-byte error page in 4 of 6 observed builds (`dev-tier/…__2` L85 `341 … /app/doom.wad`); all 5 passes fetched doom1.wad from raw.githubusercontent (e.g. `dev-tier/…__2` L112) | qemu MIPS source as reference (`dev-tier/…__3` L202–224); web searches for the WAD | none (reference source, not benchmark material) | image flakiness cannot be fixed without an image change |
| path-tracing (9) | none observed | none (0 hits) | none | — |
| chess-best-move (20) | none proven: `r3-smoke/…__1` passed with 0 installs | `pip install python-chess`/`numpy`, `apt-get install librsvg2-bin` in 19/20 | **attempted, none obtained**: lichess cloud-eval/explorer queried in 13 trials, every reply 401/404/URL error (e.g. `baseline-smoke/…__1` L57, L83) | whether python-chess is needed in practice (19/20 used it) |
| crack-7z-hash (9) | **apt-get install p7zip-full libcompress-raw-lzma-perl** in 9/9 (image builds john only: Dockerfile L6–9), e.g. `dev-tier/…__1` L18–24 | wamerican, libssl-dev | none | — |
| extract-elf (20) | none; 15 traces with no network call passed | `curl -I google.com` connectivity checks, `npm info`, duckduckgo | none | — |
| git-multibranch (20) | loopback https://localhost:8443 (image installs nginx/ssh at build) | `apt-get install sshpass` in 3 | none | — |
| headless-terminal (20) | **pip install ptyprocess/pexpect/pyte** in 19/19 passes (instruction: "Install dependencies into the system python"), e.g. `dev-tier/…__3` L40; the one trial without it is the killed `c1-audit` trial | pypi.org JSON queries | none | whether stdlib `pty` would suffice — never observed |
| kv-store-grpc (20) | **pip install grpcio==1.73.0 grpcio-tools==1.73.0** in 20/20, required by instruction step 1 (e.g. `dev-tier/…__1` L6); loopback :5328 | — | none | — |
| constraints-scheduling (9) | none; 5 traces with no network call passed | `pip install icalendar` in 4 | none | — |
| custom-memory-heap-crash (6) | none observed | none (0 hits) | none | — |

Summary: 8 tasks have no observed runtime network use; 5 tasks' observed use is optional; 7 tasks require package or data downloads at runtime (kv-store-grpc by instruction; headless-terminal, configure-git-webserver, sparql-university, crack-7z-hash in every recorded pass; make-mips-interpreter's WAD because the image build fetch is broken); 2 tasks show confirmed (db-wal-recovery) or attempted (chess-best-move) answer acquisition. All 20 need network at verification time.

## 2. Boundary test — `check_network_boundary_docker.py`

New diagnostic beside the existing `check_gemini_relay_docker.py` (which still passes this session). Same network-none container (`python:3.13-slim`, `--read-only --cap-drop ALL --security-opt no-new-privileges`), the same pipe relay with a mocked upstream and fake key `HOST_ONLY_FAKE_KEY`, plus a host-side synthetic "answer" HTTP server that the host verifies it can reach before probing. Observed results (`python3 -B harness/check_network_boundary_docker.py`, **PASS**):

| Probe | Result |
|---|---|
| Container identity | `NetworkMode=none`, networks `["none"]`, no IP; `/sys/class/net == ['lo']` |
| Python `socket.connect` to host synthetic endpoint (direct IP) | denied, `ENETUNREACH` |
| Python connect to TEST-NET IP; `urlopen` hostname; `gethostbyname` | denied (unreachable / name resolution failure) |
| In-container loopback server redirecting (302) to external IP / hostname | denied after following the redirect |
| bash `/dev/tcp/<ip>/<port>` and `/dev/tcp/<name>/80`; `getent hosts` | denied |
| `pip download ptyprocess` | denied |
| Loopback HTTP server, loopback TCP, loopback redirect into the relay gateway | allowed |
| Mocked model completion through the relay | 200, key absent from body, env has no `GEMINI*`, `/proc/1/environ` has no key |
| Mocked upstream failure | candidate sees 502; ledger keeps the full reservation charged (`cost(MAX_INPUT, MAX_OUTPUT)`), success settles to `cost(10,3)`; nothing treated as zero |
| Shutdown | `docker rm -f` ends the pipe; host worker exits with no error; ledger unchanged; synthetic server recorded zero hits |

Not covered: the production `gemini_trial_relay.control` shutdown path and host scorers (`check_budgeted_trial_docker.py` exists but was not run this session); IPv6-only or `--internal` network variants; anything a candidate could do with capabilities the production `start_isolated` still grants (it does not pass `--read-only`).

## 3. Scoring assessment

Ordinary lifecycle (`trial.rs:421-461`): after the agent exits, `rm -rf /tests /logs/verifier`, `docker cp tests → /tests`, then `docker exec … timeout … bash -c "bash /tests/test.sh > /logs/verifier/output.txt"` as root **in the candidate-modified container**, then `cat /logs/verifier/reward.txt`. Candidate-controlled inputs to that verdict:

- Binaries the verifier resolves from the container: `timeout`, `bash`, `python3`, `pip`, `apt-get`, `curl`, `sh`, `pytest`/`uv` once installed — any can be replaced or shadowed (`docker.rs:196-204` runs the container's own `timeout`).
- Background processes the agent leaves running (allowed and common: servers for kv-store, nginx, sshd) can watch `/logs/verifier` and rewrite `reward.txt` between `test.sh` writing it and the host reading it, or tamper with `/tests` after the copy.
- Network-facing verifier steps: `/etc/hosts`, resolver config, CA bundle and apt/pip configuration decide what `curl astral.sh`, `apt-get` and `pip` actually fetch and trust.
- Filesystem: `rm -rf /tests` can be defeated by an immutable attribute or bind trick when caps are present; the workspace under test is by definition candidate-written.
- `trial.rs:420` states it: "The container is a benchmark environment, not a security sandbox for root."

Demonstrated by tests: the host-scored paths (`output-line`, `polyglot`, `vim-macros`; `trial.rs:249-416`) run embedded Python on the host in `-I` mode and read artifacts via `artifact_capture`, so candidate files cannot replace scorer modules, and `evolve.rs:844-851` refuses budgeted runs for any task without such a scorer. None of the 20 dev tasks has one (`tests/` contain only `test.sh`, `test_outputs.py` and data). Network isolation therefore says nothing about scoring integrity for this slice; that gap is unresolved and not bypassable by the existing guard.

## 4. Engineering decision

**No.** Concrete incompatibilities with a network-none candidate on the existing runner:

1. All 20 verifiers need egress at scoring time (uv installer, apt, pip). Running them under the isolated path is impossible without editing `test.sh` (prohibited) or moving scoring to host scorers that do not exist for these tasks.
2. Seven tasks require runtime downloads (§1). A network-none agent phase turns these into guaranteed failures that are not model regressions; excluding them is prohibited.
3. `make-mips-interpreter` needs a runtime WAD only because the image build's fetch is unreliable; fixing that is an image change.
4. Even with network solved, ordinary scoring runs in the candidate-modified container (§3).

A domain allowlist that admits raw.githubusercontent/api.github would re-open exactly the observed contamination path; a tool-text filter is not enforcement. Both rejected.

**Recommended next experiment (bounded, $0): phase-split networking, measured with a synthetic agent.**

- Agent phase: start the task container on a Docker `--internal` bridge (no external route, loopback and container-local services intact; unlike `none`, it can be connected to another network later). Model traffic through the existing pipe relay. Extend the boundary check to this network mode.
- Verify phase: after the agent exits and before `verify()`, `docker network connect bridge <container>` so `test.sh` can perform its own installs. This preserves every verifier unchanged.
- Package requirement: run the seven runtime-download tasks' *verifiers only* against pristine images through the existing runner with the synthetic no-op agent (`check_budgeted_trial_docker.py` pattern: `--no-build --binary <synthetic>`), to confirm each verifier reaches a reward under the reconnected phase. Separately measure a host-enforced package-index relay (Debian/PyPI only, host proxy over the same pipe design, never GitHub) as the only candidate for tasks 2 and 3 above; report it as an allowlist with its residual risk, not as a solved boundary.
- Acceptance: every agent-phase probe denied on `--internal`; every loopback probe allowed; all 20 verifiers reach a reward (0 is fine, an error is not) in the reconnected phase with an untouched workspace; accounting unchanged; container removed in every exit path. Stopping criteria: `network connect` fails on a running container, any verifier errors in the reconnected phase, or the package relay cannot be expressed without admitting arbitrary hosts — report the incompatibility and stop.

## Implications for the audit's conditional paid evaluation

The conditional two-arm comparison cannot be scheduled: no candidate exists that preserves the seven required-download tasks, and any candidate that drops them would produce solve-rate losses unrelated to the coding model. Scoring integrity remains unresolved for all 20 tasks regardless of networking, so a control-vs-candidate comparison on this slice would still be scored inside candidate-modified containers on both arms. Budget position is unchanged: $0 spent this session; the reconciliation gap and the $670 escalation line from the audit stand.

## Validation in this session

Ran: `python3 -B harness/check_gemini_relay_docker.py` (PASS), `python3 -B harness/check_network_boundary_docker.py` (PASS, report above). For the pin move: `cargo check --workspace --all-targets`, `cargo fmt --all`, `cargo clippy --workspace --all-targets` (no warnings), `cargo test --workspace` (380 passed, 0 failed), `RIGCODER_EVIDENCE=write cargo test -p rigcoder --test gemini_observe` (71 passed), `cargo run --locked -p rigcoder-verify -- verify` (42/42). Not run: `check_budgeted_trial_docker.py`, any paid model call, any holdout task. `docker context ls` shows `default` (OrbStack socket path) rather than a named `orbstack` context; Docker server 29.4.0 linux/arm64.
