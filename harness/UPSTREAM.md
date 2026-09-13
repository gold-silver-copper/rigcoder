# Upstream Rig work

Current pin: `3c4346318198f92583631936d73dd8d3e7f392ce` (Rig `main`, the #2502 squash;
taken 2026-09-13).

| PR | what | status |
|---|---|---|
| [#2498](https://github.com/0xPlaygrounds/rig/pull/2498) | restore `ProviderCassette::start_at` and `checkpoint_recording`, removed by #2496 as "no callers" (rigcoder-verify is the out-of-tree caller) | merged 2026-09-12, in the current pin |
| [#2500](https://github.com/0xPlaygrounds/rig/pull/2500) | `block_reason=OTHER` is a transient `ProviderResponse`, not a refusal; rig-ecs re-issues a completion lost to a retryable provider failure inside the run (`ProviderRetries`, default 3), never re-running tools; witness fact `rig-ecs/agent/provider_retry` | merged 2026-09-13 as `724dce27`, in the current pin; consumed by rigcoder (session resubmission removed, run budget set from `RunSettings.provider_retries`) |
| [#2502](https://github.com/0xPlaygrounds/rig/pull/2502) | restore `observe::scrub_diagnostic` and `diagnostic_url_secrets` (removed by #2499 as "no callers"; rigcoder's failure records use them); a stream cut before its terminal record is retryable (rigcoder's session used to retry it by message text) | merged 2026-09-13 as `3c434631`, in the current pin; it also regenerated the #2501 goldens that had turned `main` red |
