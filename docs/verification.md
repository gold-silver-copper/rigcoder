# Verification policy

## Coverage map (before workflow changes)

Baseline: `2cb21ab171ec1c3d864ae4ecbeee6c6f1ba05d98`, workflow `ci`.
All five workspace packages declare no optional or default feature switches.
Commands below retain the manifest-selected dependency features. `cargo test` includes
unit tests, integration targets and library doctests; no test runner changes.

| Existing command / platform | Coverage | Destination |
| --- | --- | --- |
| Linux bubblewrap setup/probe (formerly both Ubuntu jobs) | Process isolation for product/repair tests | Ubuntu `offline verification`; host-only `check` no longer repeats setup |
| macOS `sandbox-exec` probe | Fail closed if sandbox is unavailable | Unchanged macOS verifier, routine and full |
| Ubuntu UI development package installation | Native dependencies for workspace build/Clippy/UI tests | Unchanged `check`, routine and full |
| `cargo check --locked --workspace` / Ubuntu | Default build of all five packages, including UI dependencies | Routine and full `check` |
| `cargo fmt --all -- --check` / Ubuntu | Entire workspace formatting | Routine and full `check`, first Cargo check |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` / Ubuntu | All workspace targets, including verifier and integration-test code | Routine and full `check` |
| `cargo test --locked --workspace --exclude rigcoder-verify` / Ubuntu | Product library/doctests, five product integration targets, bench binary/regressions, CLI and UI binaries | Product tests remain in Ubuntu `offline verification`; `check` retains bench/CLI/UI tests using a second exclusion |
| `cargo build --locked --release -p rigcoder-cli` / Ubuntu | Optimized CLI and dependency compilation | Full `check`, manual dispatch and nightly; explicit candidate full gate before release |
| `cargo test --locked -p rigcoder -p rigcoder-verify` / Ubuntu and macOS | Product library/doctests and integrations; verifier binary tests, `consumer`, `product_configuration`; platform-specific process, filesystem and repair isolation | Unchanged on both platforms, routine and full |
| `cargo run --locked -p rigcoder-verify -- verify` / both | Complete registry: producers, replay, applicable resume cuts, cassette matching and semantic evidence | Unchanged on both platforms, routine and full; no fixed case count |

The product integration targets are `gemini_blocked_prompt`, `gemini_observe`,
`observe_matrix`, `replay`, and `steer`. `gemini_observe` also invokes Cargo for
the CLI and bench binaries in the workspace target directory. Those builds are
not pure test execution. Repair validation intentionally uses disposable,
isolated Cargo homes and target directories: do not share their artifacts or
weaken sandboxing to accelerate them. The standalone verifier is distinct from
unit tests even though both consume the registry.

## Local, routine and full gates

| Tier / trigger | Commands and scope |
| --- | --- |
| Local edit | `cargo fmt --all -- --check`; `cargo test --locked -p PACKAGE [FILTER]`; `cargo clippy --locked -p PACKAGE --all-targets -- -D warnings`. Include affected consumers when changing shared code. |
| Local product evidence | `RIG_PROVIDER_TEST_MODE=replay cargo test --locked -p rigcoder --test gemini_observe`; choose the applicable integration target for other product contracts. |
| Local verifier evidence | `cargo run --locked -p rigcoder-verify -- plan`; `cargo run --locked -p rigcoder-verify -- verify --case CASE_ID` (or `--matrix MATRIX`). Select by contract using the consumer guide; these focused commands do not certify a release. |
| Routine PR; push to main/evolve | Ubuntu formatting, workspace check/Clippy, bench/CLI/UI tests; Ubuntu **and macOS** product/verifier tests and complete standalone verifier. Exact commands are in `.github/workflows/ci.yml`. |
| Full manual; daily schedule | Entire routine matrix plus `cargo build --locked --release -p rigcoder-cli` in Ubuntu `check`. |
| Release candidate or release-specific code/configuration | Dispatch full CI at the exact candidate revision and require all three jobs to succeed before publication. There is no release workflow. An older scheduled success is insufficient. |

Run the full local command set (on Ubuntu install the UI development packages
listed in CI and run `bash .github/scripts/setup-verifier-sandbox.sh`; on macOS
check `sandbox-exec -p '(version 1)(allow default)' /usr/bin/true`):

```sh
export RIG_PROVIDER_TEST_MODE=replay
cargo fmt --all -- --check
cargo check --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --exclude rigcoder --exclude rigcoder-verify
cargo test --locked -p rigcoder -p rigcoder-verify
cargo run --locked -p rigcoder-verify -- verify
cargo build --locked --release -p rigcoder-cli
```

Routine local verification omits only the last command. Local focused checks
are useful during editing; the full platform matrix remains the publication
contract. Dependency pin changes still require workspace tests and the full
verifier as described in the README. PR review requirements remain in force.
There is no new requirement to rerun every unchanged check after every edit.

To certify a candidate, push an authorized candidate branch, then run:

```sh
gh workflow run ci.yml --ref CANDIDATE_BRANCH
gh run list --workflow ci.yml --event workflow_dispatch --branch CANDIDATE_BRANCH
gh run view RUN_ID --json headSha,status,conclusion,jobs
```

Confirm `headSha` is the intended commit and all three jobs succeeded; retain
the two verification reports. Run local release compilation before publishing
changes that affect release configuration, and complete this hosted gate before
releasing that revision. No provider credentials or recording are needed.

## Scheduling, caches and failure behavior

No path selector is needed: product, verifier-only, UI/CLI, fixture, dependency,
shared-config and docs-only edits all select the same routine matrix. Thus
unknown paths, renames and deletions cannot silently evade coverage. Full manual
and scheduled runs add release compilation regardless of changed paths.

PR concurrency groups include workflow and PR ref; only superseded runs of the
same PR cancel. Push, schedule and manual runs have unique run groups, so they
do not cancel one another. Existing check names are preserved. At preflight,
main had no branch protection and no repository rulesets; no settings change
is needed or performed.

The existing Rust cache remains job-, OS-, architecture- and compiler-specific,
with manifest/lock/config hashes and dependency-only target caching. Only main
pushes, schedules and manual runs save trusted caches; PRs and manual runs on
other candidate branches restore without saving. The Ubuntu `check` cache has
separate routine/full keys so an immutable routine cache cannot prevent saving
release dependencies. The unused preinstalled `stable` toolchain is removed after
installing 1.95.0: rust-cache hashes *all* installed compilers, and varying runner
stable versions otherwise invalidate caches even though no command uses them.
The first run after this correction needs to populate new keys. No cache hit
skips tests, and independent jobs do not share a target filesystem.

No tests, assertions, sandbox checks, cassette behavior or compiler profiles
change. Failed commands fail their job; there are no tolerated failures or
retries. Verifier reports upload with `always()`, including after failures.

## Measurement

Before: run `34324262897`, attempt 2, commit `2cb21ab`, on hosted
`ubuntu-latest` (x64) and `macos-latest` (arm64). Attempt 1 never executed
because of private-repository billing and is excluded.

Completed Ubuntu `check`: 908 seconds job wall time, with a 1,265 MB fallback
cache hit; check 21s, formatting 1s, Clippy 7s, tests 711s, release build 96s,
cache restore 18s/save 30s. Tests include 393s top-level compilation and a
313s product evidence target dominated by nested CLI/bench builds.
Ubuntu verification: 1,182 seconds job wall time, **cache miss**; tests 1,109s
(646s compilation; product evidence target 384s), standalone verifier 24s,
cache restore 2s/save 17s. Removing duplicate product execution saves work,
but those parallel durations must not be added to claim latency savings.

Earlier successful run `34174977600` at `65b5917` took 530s (`check`), 756s
(Ubuntu verifier) and 875s (macOS verifier): critical span 878s, total runner
2,161s. It predates substantial product evidence additions and is not a
comparable warm performance baseline. macOS restored a cache and spent 297s
compiling, plus 417s in the two verifier test targets. Fresh same-revision
measurements are needed before claiming a warm critical-path improvement.

The completed baseline macOS job took 1,658s with a fallback cache hit:
1,504s in `cargo test` (525s top-level compilation, 569s product evidence
including nested builds, 402s in verifier test targets), 85s standalone verifier,
18s cache restore and 30s save. Baseline critical span, from the first job start
to the last completion, was **1,666s (27m46s)**; summed runner time was
**3,748s (62m28s)**. This is a mixed-cache baseline, not a fully warm benchmark.
The workflow change removes redundant Ubuntu work; macOS remains on the critical
path. Report final-revision hosted results separately, including initial cache
population, rather than claiming the removed work equals elapsed-time savings.
