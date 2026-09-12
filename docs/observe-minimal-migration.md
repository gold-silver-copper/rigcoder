# Reduced Rig observation integration

This integration supersedes the API used by Rig #2476. Ordinary providers
implement `completion` and `stream`. Optional observed methods default to those
methods without provider facts; Gemini overrides them. Context stays separate
from request data and is forwarded per dispatch, never through shared mutable
client state.

## Consumer accounting

| Behavior from #2476 | Decision and current consumer |
| --- | --- |
| Hold owners, batch concurrency and Gemini error classification | Retained as execution correctness, independent of an installed witness. World scenes now retain both the batch hold marker and all named owners. |
| Landed, Denied, Replaced, effect/scope correlation | Retained: product failure reporting identifies the failed completion or host decision without attributing an unrelated failure to a provider. |
| Issued, Held, Released, Refused, Cancelled, StreamTruncated, Ended | Retained: dispatch/hold diagnostics and run/stream failure summaries; hold history also provides artifact context. |
| Provider operation, attempt, status, closure, typed error boundary, verdict and error envelope | Retained: provider failure attribution and retry diagnosis. A provider closure is never task success. |
| Provider usage, EOF/corrupt-frame facts, response IDs and allowlisted headers | Retained: bounded provider diagnostic evidence. Digest keeps optional usage per attempt. Trusted budget accounting remains in the Python Gemini gateway; missing usage is not zero. |
| Host approval, failure and retry decisions | Retained in rigcoder's existing host facts and reports. Runtime cancellation reason and invalid-call failure remain in the product failure report. |
| Approved, Patched, Deferred, CancelRequested, Retry, InvalidCall runtime events | Dropped: no production report/scoring/selection consumer. Host approvals/retries and product failures provide actionable diagnostics; final patched requests remain in effect records. Dispatch behavior is unchanged. |
| Run, handler and provider interval instrumentation, response-byte observer | Dropped: only stored in artifacts or asserted in timing tests; no production calculation or report consumer. Optional individual timestamps and recording provenance remain. |
| Public runtime semantic trace comparison | Moved to test support: Rig's test-utils feature and rigcoder integration tests. The standalone verifier keeps its own existing evidence comparison. No new execution-equivalence guarantee. |
| Benchmark aggregation, presentation and measurement labels | Stay in rigcoder. Scoring, improvement-lane selection, sandboxing and the trusted budget gateway are unchanged. |

## Fixture projection

The migration explicitly removes only the dropped runtime events from frozen
observation fixtures and renumbers the retained events in their original order.
Across 22 changed evidence observation files, it removes 3 invalid-call,
5 cancellation-request, 1 runtime retry, 6 scheduling deferral and 6 patch events,
plus 12 run, 15 handler and 13 adapter timing fields. Companion `run.json`
observation counts are updated to the retained event count. The invalid-tool
expected trace receives the same event projection.

Provider cassettes, effect records, transcripts, histories and workspace results
are not regenerated. The retained facts still compare exactly after the existing
normalization of diagnostic timestamps, provider analysis and recording context.
Unknown events are not silently discarded during replay comparison. Scheduling,
patching, cancellation, retry and invalid-tool tests retain their independent
execution assertions; assertions exclusively about deleted events/intervals are
removed. Tests still distinguish absent usage, failed attempts, incomplete
traces and provider closure from execution success.

Verification and publication results are reported separately after the final Rig
revision is published and tested through exact Git dependency pins.

The five direct Rig Git dependencies and their transitive Rig packages are pinned
to the head of [Rig PR #2443](https://github.com/0xPlaygrounds/rig/pull/2443),
revision `ef4cd8c15ef2001eb50a3a46bb9cbefb48f1780a`, which now includes the merged
replacement [Rig PR #2482](https://github.com/0xPlaygrounds/rig/pull/2482). Evidence `cell.json` Rig revision labels
and the test producer constant identify the revision used for the migrated replay
verification. This metadata update does not rerecord provider cassettes.
