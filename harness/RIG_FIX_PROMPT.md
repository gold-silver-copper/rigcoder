# Task: fix two provider-failure defects in Rig, found by rigcoder's dev tier

You are working in a full clone of `0xPlaygrounds/rig` on a branch from
`main`. Two Terminal-Bench failure modes traced to Rig, not to the
consumer. Fix them upstream. The consumer is rigcoder
(`gold-silver-copper/rigcoder`), which pins Rig by git revision and is the
out-of-tree caller of `rig-ecs`, `rig-effect-log` and `rig-cassette`;
rigcoder-verify moved out of this repo in #2474, so an in-tree "no
callers" audit does not see it.

Ground rules: read `CONTRIBUTING.md` and `crates/rig-ecs/CONTRACT.md`
first. Conventional Commit titles. Never edit `CHANGELOG.md` or
`MIGRATING.md`; put `## Changelog` and `## Migration` in the PR body. The
workspace forbids `unwrap`, `expect`, `todo` outside tests. Fast loop:
`cargo xtask verify --changed`; before opening a PR:
`cargo xtask verify --pr --base main`. Report only what you ran.

One PR, one branch `fix/provider-failure-retry`, two commits in this
order so the review can read them separately: the classification fix,
then the run-level retry. The second part changes runtime behaviour and
gets a design note in the PR description before code.

## Part 1: a Gemini `block_reason=OTHER` is not a refusal

### Observed

Two of three dev-tier attempts of one task ended on the first request:
`ProviderError: Gemini blocked the prompt: block_reason=OTHER`, with
`retryable: false`. The third attempt, same prompt, same model, passed.
Google documents `OTHER` as "blocked due to unknown reasons"; the
content-based reasons are `SAFETY`, `BLOCKLIST` and `PROHIBITED_CONTENT`.

### Where

- `crates/rig-core/src/providers/gemini/completion.rs`,
  `blocked_prompt_error` (around line 650): every `BlockReason` except
  `BlockReasonUnspecified` becomes one
  `CompletionError::ProviderError(String)`. The variant enum is at
  about line 1799 and includes `Unknown(String)` for wire values this
  crate does not know.
- `crates/rig-core/src/error.rs`: `ErrorReport` has `retryable` and
  `refusal` fields (about line 139), and `From<CompletionError>` sets
  them from `is_retryable()` / `is_refusal()`. Find where a
  `ProviderError` is classified and how a refusal is recognised today.
- Existing pins to keep green:
  `crates/rig-core/src/providers/gemini/streaming/tests.rs` ~868 asserts
  a blocked prompt is `ErrorKind::Provider` and not retryable;
  `crates/rig-core/src/providers/gemini/completion/tests.rs` ~41 asserts
  the message names `SAFETY` and carries the ratings. Those tests use
  `SAFETY`; they must stay exactly as they are.

### Required behaviour

- `SAFETY`, `BLOCKLIST`, `PROHIBITED_CONTENT`: unchanged. A refusal, not
  retryable, message names the reason and the safety ratings.
- `OTHER`: not a refusal, `retryable: true`. The message still names the
  reason. Do this with a typed distinction the classifier can read, not
  by matching the message text downstream; if `CompletionError` needs a
  way to carry "provider blocked, transient" versus "provider refused",
  add the smallest one and use it in `is_retryable` / `is_refusal`.
- `Unknown(String)`: treat like `OTHER` (unknown means unknown), and say
  so in a doc comment.
- Both the unary and the streaming path (the streaming path reads the
  first chunk's `promptFeedback`; see the streaming tests) produce the
  same classification.

### Tests

One test per reason on each path, asserting `retryable` and `refusal`
on the resulting `ErrorReport`, in the existing test files next to the
pins above. Use the cassette fixtures' JSON shape for the blocked chunk;
`rigcoder`'s `crates/rigcoder/tests/gemini_blocked_prompt.rs` has a
documented example of the chunk if you need one.

## Part 2: retry a failed completion inside the run

### Observed

A dev-tier attempt of `password-recovery` had made 37 tool calls when
Gemini answered a completion with HTTP 503. Rig classified it retryable
(`retryable_status` in `crates/rig-core/src/error.rs` ~252 covers 503).
Nothing in Rig re-issued the completion; the run ended
`Failed(Failure::Provider(report))`. The consumer's only retry is
resubmitting the whole prompt from the session, which would re-run the
37 tools, so it refuses once a request has produced tool work
(rigcoder `crates/rigcoder/src/session.rs` ~500, comment "A
whole-prompt retry is safe only before this request has produced tool
calls"). Result: a transient 503 late in a run is fatal, and the retry
budget exists only for the first request.

### Where

- `crates/rig-ecs/src/systems/mod.rs`: the completion effect's failed
  outcome becomes `Failed(Failure::Provider(report))` on the run
  (around line 1679; other `Failure::Provider` insertions near 780 and
  801 are binding failures, not provider replies, and stay as they are).
- `crates/rig-ecs/CONTRACT.md` §5 (budgets and endings) and §9.4 (the
  `Retry` resolution). `Retry` retries a *complete, tool-free turn* on a
  judge's decision; it is not the primitive for a completion that never
  produced a turn. Do not overload it.
- `crates/rig-effect-log`: the effect log already records a failed
  attempt and a successful one for the same operation; rigcoder's
  `observe_wire/retry_*` evidence cells show the shape (attempt and
  host_attempt facts, `rigcoder/provider_retry`). Replay must reproduce
  both attempts in order.

### Required behaviour

- When a completion effect settles with a provider `ErrorReport` whose
  `retryable` is true (after PR 1 that includes `block_reason=OTHER`),
  and the run's retry budget is not exhausted, the run re-issues the
  *same* completion request over the *same* assembled history after a
  backoff. No tool is re-executed; no history is rewritten; the turn
  that failed leaves no assistant message. Non-retryable reports end the
  run exactly as today.
- The budget is a setting on the run or agent, like the existing
  budgets in §5, default 3, resettable by the consumer. Backoff is
  wall-clock in live mode and zero under replay, the way rigcoder's
  session retry already distinguishes `Mode::Replay`.
- Every attempt is a record in the effect log with the attempt number;
  a checkpoint taken between attempts resumes into the retry, not into
  a fresh prompt. Cancellation during backoff ends the run `Cancelled`.
- Observability: emit a fact for the retry decision (operation, attempt,
  wait, reason) through the existing witness path so consumers can
  count retries per trial without parsing messages.

### Design note first

Write the §5 addition to `CONTRACT.md` and a short "why not `Retry`"
paragraph in the PR description before implementing. If this needs a
change to the bus contract or to how `Failed` is observed, stop and
describe the options instead of choosing one. Part 1 is still committed
on the branch either way; it must not wait on part 2's design.

### Tests

- A scripted-provider test in `rig-ecs`: 503 after two tool batches,
  then success; the tools ran once, the answer settles, the effect log
  has both attempts, replay reproduces them.
- Budget exhausted: three retryable failures end the run
  `Failed(Provider)` with the last report.
- Non-retryable failure after tool work: ends the run on the first
  failure, as today.
- Cancel during backoff: `Cancelled`, no further request.
- Resume from a checkpoint taken during backoff continues with the
  retry.

## The PR

Title: `fix(gemini, rig-ecs): retry transient provider failures inside
the run; block_reason=OTHER is not a refusal`. Body, per the template:

- Description: both observed failures with their repros (dev-tier
  trials via rigcoder-bench: `fix-code-vulnerability` attempts 2 and 3,
  first request, zero tool calls, block `OTHER`; `password-recovery`
  attempt 2, HTTP 503 after 37 tool calls), the expected behaviour for
  each, the Google doc line for `OTHER`, the "why not `Retry`" note and
  the new §5 text.
- `## Changelog`: one bullet under `*(gemini)*` for the classification,
  one under `*(rig-ecs)*` for the run-level retry and its budget
  setting, marked `[**breaking**]` only if a public type changed shape.
- `## Migration`: how a consumer that resubmitted prompts itself turns
  that off and sets the run budget instead; `None` for the Gemini part
  unless a public variant was added.
- Testing: only commands you ran, with their results.

### After merge, in rigcoder

Move the pin (rigcoder `harness/ITERATE_PROMPT.md` §4 has the
procedure), delete the whole-prompt retry in `session.rs` in favour of
the run-level budget, keep its tests as consumer-side pins of the new
behaviour, and regenerate the `observe_wire/retry_*` evidence cells.
Then a paired smoke run.
