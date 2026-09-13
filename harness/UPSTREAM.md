# Upstream Rig work

Current pin: `7830f83399ede81adc9f62370073e3ca196c1de2` (head of PR #2502: Rig
`main` at `724dce27` plus its two fixes; taken 2026-09-13; return to `main`
when #2502 merges, the `TODO` in `Cargo.toml` says so).

| PR | what | status |
|---|---|---|
| [#2498](https://github.com/0xPlaygrounds/rig/pull/2498) | restore `ProviderCassette::start_at` and `checkpoint_recording`, removed by #2496 as "no callers" (rigcoder-verify is the out-of-tree caller) | merged 2026-09-12, in the current pin |
| [#2500](https://github.com/0xPlaygrounds/rig/pull/2500) | `block_reason=OTHER` is a transient `ProviderResponse`, not a refusal; rig-ecs re-issues a completion lost to a retryable provider failure inside the run (`ProviderRetries`, default 3), never re-running tools; witness fact `rig-ecs/agent/provider_retry` | merged 2026-09-13 as `724dce27`, in the current pin; consumed by rigcoder (session resubmission removed, run budget set from `RunSettings.provider_retries`) |
| [#2502](https://github.com/0xPlaygrounds/rig/pull/2502) | restore `observe::scrub_diagnostic` and `diagnostic_url_secrets` (removed by #2499 as "no callers"; rigcoder's failure records use them); a stream cut before its terminal record is retryable (rigcoder's session used to retry it by message text) | open 2026-09-13; pinned at its head |
