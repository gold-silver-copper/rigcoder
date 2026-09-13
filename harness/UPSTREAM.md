# Upstream Rig work

Current pin: `896bb8b4c62a21df9bb97a5973216c41ed995001` (Rig `main`,
taken 2026-09-12).

| PR | what | status |
|---|---|---|
| [#2498](https://github.com/0xPlaygrounds/rig/pull/2498) | restore `ProviderCassette::start_at` and `checkpoint_recording`, removed by #2496 as "no callers" (rigcoder-verify is the out-of-tree caller) | merged 2026-09-12, in the current pin |
| [#2500](https://github.com/0xPlaygrounds/rig/pull/2500) | `block_reason=OTHER` is a transient `ProviderResponse`, not a refusal; rig-ecs re-issues a completion lost to a retryable provider failure inside the run (`ProviderRetries`, default 3), never re-running tools; witness fact `rig-ecs/agent/provider_retry` | open 2026-09-12; on merge, follow `HARNESS_FIX_PROMPT.md` commit 3 |
