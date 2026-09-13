# Task: make a program's recorded identity data, not only a hash (Rig)

You are working in a full clone of `0xPlaygrounds/rig` on a branch from
`main`, opening one PR. Read `CONTRIBUTING.md` and `crates/rig-ecs/CONTRACT.md`
§10 ("Identity as data") first. Conventional Commit titles; never edit
`CHANGELOG.md` or `MIGRATING.md`; `## Changelog` and `## Migration` go in
the PR body. `cargo xtask verify --changed` for the loop,
`cargo xtask verify --pr --base main` before opening. Report only what ran.

## The problem

A replay is refused when the log was recorded under a different policy
than the program replaying it. That refusal is right and stays. It is
keyed on `ProgramIdentity.policy: u64` (`crates/rig-effect-log/src/log.rs`
~74), the `stable_hash` (same file, ~283: FNV over canonical JSON) of
`spec_json(world, run)` (`crates/rig-ecs/src/replay/mod.rs` ~107), written
per run scope by `stamp_run` (~354) into `LogHeader::programs`, and checked
by `check_replayable` (~389), whose refusal reads
`replay refused: the log was recorded under policy 0x…, this agent's is 0x…`.

The hash is the only committed representation of what it hashes. Three
consequences, all seen on 2026-09-13:

- **Churn is opaque in review.** #2500 added `provider_retries` to
  `spec_json`; every golden's `/header/programs/*/policy` moved, 107
  files in that PR, and a reviewer sees 107 integers change. Nothing in
  the diff says "provider_retries: 3 appeared".
- **Regeneration is only as complete as the test selection the author
  ran.** The first regeneration on #2500 covered three modules; the
  pre-publish verify found the rest. The mechanism is
  `RIG_REGENERATE_GOLDEN=1` in `tests/common/goldens.rs` (~40), which
  re-records the whole golden from its producer to change one integer.
- **PRs cannot cross.** #2501 was authored on a base before #2500 and
  added 491 goldens for five wires with the old hash. It merged after
  #2500 and `main`'s own `stable / test` went red; #2502's merge ref
  inherited that, and the fix (regenerate 491 files, hash only) landed
  in an unrelated PR. There is no rebase-time signal, only CI.

## Required behaviour

1. **The identity is stored as data.** `ProgramIdentity` gains the policy
   JSON it was hashed from: `policy_spec: serde_json::Value` (the
   canonical `spec_json`), beside `policy: u64`. `stamp_run` writes
   both. Keep `policy` in the header so nothing that reads it today
   changes; it is derived from `policy_spec` and a header whose two
   disagree is refused at load as corrupt, with a message that says so.
2. **The refusal names the difference.** `check_replayable` compares the
   recorded `policy_spec` to the live `spec_json` and reports the
   differing keys and both values (`provider_retries: recorded absent,
   live 3`), not two hashes. Keep the hash comparison as the fast path;
   the readable diff is what the error carries. Secrets never enter
   `spec_json` today; add a test that pins that (no `api_key`, no URL
   userinfo) so storing it as data does not widen what a log leaks.
3. **Identity churn regenerates without re-recording.** A tool under
   `xtask`, `cargo xtask goldens restamp`, loads every golden under
   `crates/rig-verify/fixtures`, rebuilds the program from the golden's
   own producer declaration where one exists or, where it does not,
   recomputes only the fields the identity schema derives from the
   stored spec (see 4), rewrites `policy_spec` and `policy`, and touches
   nothing else in the file. It prints one line per file with the key
   diff. Regenerating a golden by re-recording stays the path for a
   behaviour change; restamping is for an identity change.
4. **Identity schema version.** `ProgramIdentity` gains
   `schema: u32`, bumped whenever a field is added to or removed from
   `spec_json`. A golden with an older schema is refused with
   `identity schema 2, log has 1: run cargo xtask goldens restamp`, before
   the hash is compared. `restamp` knows how to lift each old schema to
   the current one (for #2500's change: insert `provider_retries` at its
   default, 3, then rehash), so a PR authored on an older base is fixed
   by one command after rebase, and the lift is reviewable code.
5. **A CI guard for crossing PRs.** A test in the root suite loads every
   golden and asserts `schema` is current and `policy ==
   stable_hash(policy_spec)`. It fails with the file list and the
   restamp command, so a golden recorded on an older base is caught on
   the PR that rebases it, with a one-command fix, instead of turning
   `main` red at merge.
6. **Contract.** CONTRACT §10 gains the rows: identity is data and hash;
   the hash is derived; schema version and restamp; the refusal names
   keys. Update the `goldens.rs` module doc: goldens are re-recorded by
   their producer for behaviour, restamped for identity.

Out of scope: changing what is in `spec_json`, changing the hash
function, or making any field conditional on whether it affected the
recorded effects (that was option 2; the schema version and restamp make
it unnecessary).

## Where

- `crates/rig-effect-log/src/log.rs`: `ProgramIdentity`, `LogHeader::programs`,
  `stable_hash`, `Canonical`. Serde: `policy_spec` and `schema` need
  `#[serde(default)]` so existing goldens load, then the schema check
  refuses them with the message in 4.
- `crates/rig-ecs/src/replay/mod.rs`: `spec_json`, `spec_hash`,
  `stamp_run`, `check_replayable`, `required_row`.
- `tests/common/goldens.rs`: `golden_effects`, `RIG_REGENERATE_GOLDEN`.
- `crates/rig-verify/fixtures/ecs_parity/*.effects.json` and the other
  golden corpora under `crates/rig-verify/fixtures`: 598 files carry
  `programs` today (107 + 491). Restamp them all in this PR with the new
  tool, so the diff is the tool's output and reviewers can read it.
- `crates/rig-ecs/tests/run_identity.rs`, `run_replay_policy.rs`,
  `run_replay_metadata.rs`: the existing pins of the refusal; extend,
  do not weaken.
- `xtask/`: where `verify` lives; add `goldens restamp` beside it.

## Tests

- A recorded log with a different `provider_retries` is refused and the
  message names `provider_retries` with both values.
- A header whose `policy` does not equal `stable_hash(policy_spec)` is
  refused as corrupt at load.
- A header with `schema: 1` is refused naming the restamp command, before
  any hash comparison.
- `restamp` on a schema-1 golden produces schema 2 with
  `provider_retries: 3`, the right hash, and a byte-identical file
  otherwise; running it twice is a no-op.
- The CI guard test lists every golden and passes on the tree.
- `spec_json` contains no secret: construct a program with an API key and
  a URL with userinfo and assert neither string appears.

## PR body

Description: the three consequences above with the PR numbers as the
repro. `## Changelog`: `*(effect-log)*` identity as data and schema;
`*(ecs)*` the readable refusal; `*(xtask)*` `goldens restamp`.
`## Migration`: existing logs load and are refused with the restamp
message; consumers that stamp their own identity call the same
`stamp_run` and get both fields; a consumer with its own goldens
(rigcoder's evidence packets carry `policy` in `effects.json` headers)
restamps them with the tool or re-records. Testing: only what ran.

## Stop and ask

- If storing `policy_spec` would put anything in a log that is not
  already in `spec_json` today.
- If the restamp cannot be made a no-op on a current-schema golden.
- If lifting an old schema needs a value the old golden does not carry
  and no default is defensible; say which field.
