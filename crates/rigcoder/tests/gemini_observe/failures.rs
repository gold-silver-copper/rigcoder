//! Matrix D — provider failures (`observe_failures`).
//!
//! A failed run is a join of two facts: `Ended { provider, "<kind>: <message>" }`
//! and the `landed` summary carrying the report's kind code; every cell
//! checks the join.
//!
//! | cell | dimension pinned | oracle | facts asserted | status |
//! |---|---|---|---|---|
//! | `bad_request_{unary,stream}` | HTTP 400 (an invalid `generationConfig`) | run failed, one request, no retry | `issued`, `landed` Err (http 400, not retryable), `ended:<kind>`; no `provider_retry` | recorded |
//! | `bad_key_{unary,stream}` | an invalid API key (recorded with `bogus_api_key`) | run failed, one request | same shape; the reason names the key | recorded |
//! | `unknown_model_{unary,stream}` | HTTP 404 (a model that does not exist) | run failed, one request | same shape | recorded |
//! | `rate_limited_unary` | HTTP 429 `RESOURCE_EXHAUSTED`, then the answer | settled after one retry, two requests | `landed` Err retryable, `ended:provider`, `provider_retry { attempt: 1 }`, `issued`, `landed` Ok, `ended:settled` | derived from `text_unary` (a 429 interaction prepended) |
//! | `server_error_stream` | HTTP 503 `UNAVAILABLE`, then the answer | settled after one retry | same shape on the streamed wire | derived from `text_stream` |
//! | `blocked_prompt_{unary,stream}` | `promptFeedback.blockReason` from a live safety block | run failed once, one request, no retry | `ended:provider` naming the block reason; no `provider_retry`, no `stream_truncated` | recorded if the live API blocks; otherwise dropped here and proven by `tests/gemini_blocked_prompt.rs` (documented chunk, local server) |
//! | `stream_truncated_stream` | the stream ends before its terminal frame, then the answer | settled after one retry | `stream_truncated { delivered ≥ 1, tail non-empty }` (emitter `rig-ecs/bus`), `landed` Err, `provider_retry`, `ended:settled` | derived from `text_stream` (last frame dropped) |
//! | `stream_error_frame_stream` | an error frame after text, no terminal | the run reports the error | the error reaches the trace (`stream_truncated.errors` or a `landed` Err naming it) | derived from `text_stream` (terminal replaced by an error frame) |
//! | `malformed_frame_stream` | a frame that is not JSON | the run reports the parse failure | `landed` Err whose code is the parser's kind; no `provider_retry` | derived from `text_stream` (first frame corrupted) |
//! | `transport_cut_stream` | the body ends mid-frame | the run reports the cut | distinguishable from the clean truncation: the trace's error names the cut | derived from `text_stream` (body cut mid-JSON) |

use crate::support::*;
use rig::observe::{Action, OutcomeSummary};

const MATRIX: &str = "observe_failures";

/// The story of a failed run is a join: `Ended { ending }` names the
/// runtime's failure variant (`provider` for any report a completion
/// landed with) and carries the report's message as its detail; the
/// report's own kind (`http`, `json`, `response`, …) is on the `landed`
/// fact's summary. Returns (ending code, landed kind code, summary).
fn ending_matches_landed(cell: &Cell) -> (String, String, OutcomeSummary) {
    let landed = cell
        .trace()
        .observations
        .into_iter()
        .filter(|o| {
            matches!(
                o.action,
                Action::Landed {
                    outcome: OutcomeSummary::Err { .. }
                }
            )
        })
        .last()
        .expect("a failed landing");
    let Action::Landed { outcome } = landed.action.clone() else {
        unreachable!()
    };
    let Action::Ended { ending } = cell
        .trace()
        .observations
        .into_iter()
        .filter(|o| matches!(o.action, Action::Ended { .. }))
        .last()
        .expect("the run ended")
        .action
    else {
        unreachable!()
    };
    let OutcomeSummary::Err { reason, .. } = &outcome else {
        unreachable!()
    };
    assert_eq!(
        ending.code, "provider",
        "a landed report ends the run as a provider failure: {ending:?}"
    );
    assert_eq!(
        ending.detail.as_deref(),
        Some(
            format!(
                "{}: {}",
                reason.code,
                reason.detail.clone().unwrap_or_default()
            )
            .as_str()
        ),
        "the ending carries the report's kind code and message verbatim (lowercased before the fix fed back to #2476)"
    );
    (ending.code, reason.code.clone(), outcome)
}

fn failed_once(cell: &Cell, expect_status: Option<u16>) {
    assert_ne!(cell.ending(), "settled", "{:?}", cell.events());
    let facts = cell.facts();
    assert_eq!(facts.len(), 3, "{facts:?}");
    assert_eq!(&facts[..2], ["issued", "landed"]);
    assert!(facts[2].starts_with("ended:"), "{facts:?}");
    assert_eq!(cell.count("rigcoder/provider_retry"), 0, "{facts:?}");
    let (code, kind, outcome) = ending_matches_landed(cell);
    let OutcomeSummary::Err { reason, retryable } = outcome else {
        panic!("{outcome:?}")
    };
    assert!(!retryable, "{reason:?}");
    let log = cell.log();
    assert_eq!(log.records.len(), 1);
    let Err(report) = &log.records[0].outcome else {
        panic!()
    };
    eprintln!(
        "[{}/{}] ended:{code}, landed {kind}, http {:?}: {}",
        MATRIX, cell.name, report.http_status, report.message
    );
    if let Some(status) = expect_status {
        assert_eq!(report.http_status, Some(status), "{report:?}");
    }
    assert_eq!(
        report.kind.code(),
        kind,
        "the landed summary is the record's kind"
    );
}

fn pair(name: &str, config: fn(bool) -> Config, body: impl Fn(&mut Cell, bool)) {
    let mut traces = Vec::new();
    for stream in [false, true] {
        let cell = format!("{name}_{}", if stream { "stream" } else { "unary" });
        run(MATRIX, &cell, config(stream), |cell| {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(cell, stream)));
            eprintln!("[{}/{}] facts: {:?}", MATRIX, cell.name, cell.facts());
            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
            traces.push(semantic_facts(&cell.trace()));
        });
    }
    assert_eq!(
        traces[0], traces[1],
        "unary and streamed agree on the facts"
    );
}

#[test]
fn a_bad_request() {
    pair(
        "bad_request",
        |stream| Config {
            retries: 3,
            additional_params: Some(serde_json::json!({
                "generationConfig": {"temperature": 99.0}
            })),
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            failed_once(cell, Some(400));
        },
    );
}

#[test]
fn a_bad_key() {
    pair(
        "bad_key",
        |stream| Config {
            retries: 3,
            bogus_key: true,
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            failed_once(cell, None);
            let log = cell.log();
            let Err(report) = &log.records[0].outcome else {
                panic!()
            };
            assert!(
                matches!(report.http_status, Some(400 | 401 | 403)),
                "{report:?}"
            );
        },
    );
}

#[test]
fn an_unknown_model() {
    pair(
        "unknown_model",
        |stream| Config {
            retries: 3,
            model: "gemini-9.9-nonexistent",
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            failed_once(cell, Some(404));
        },
    );
}

fn gemini_error(status: u16, kind: &str, message: &str) -> String {
    serde_json::json!({"error": {"code": status, "message": message, "status": kind}}).to_string()
}

/// A derived cassette: the source's first interaction answered `status`
/// with a Gemini error body, then the source's own interactions.
fn with_failure_first(
    source_name: &str,
    derived_name: &str,
    status: u16,
    kind: &str,
    message: &str,
) -> std::path::PathBuf {
    let source = cassette_path("observe_turns", source_name);
    derive(&source, &cassette_path(MATRIX, derived_name), |docs| {
        let mut failure = docs[0].clone();
        set_status(&mut failure, status);
        failure["then"]["header"] = serde_yaml::to_value(vec![
            serde_yaml::to_value(std::collections::BTreeMap::from([
                ("name", "content-type"),
                ("value", "application/json; charset=UTF-8"),
            ]))
            .unwrap(),
        ])
        .unwrap();
        failure["then"]["body"] = serde_yaml::Value::String(gemini_error(status, kind, message));
        docs.insert(0, failure);
    })
}

fn retried_then_settled(cell: &Cell, first_kind: &str) {
    assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
    let facts = cell.facts();
    let first = cell
        .find(|a| {
            matches!(
                a,
                Action::Landed {
                    outcome: OutcomeSummary::Err { .. }
                }
            )
        })
        .unwrap();
    let Action::Landed {
        outcome: OutcomeSummary::Err { reason, .. },
    } = first.action
    else {
        unreachable!()
    };
    assert_eq!(reason.code, first_kind, "the first landing's kind");
    let expected: Vec<String> = [
        "issued",
        "landed",
        "ended:provider",
        "rigcoder/provider_retry",
        "issued",
        "landed",
        "ended:settled",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(semantic_facts(&cell.trace()), expected, "{facts:?}");
    let retry = cell
        .find(|a| matches!(a, Action::Host { kind, .. } if kind == "rigcoder/provider_retry"))
        .unwrap();
    let fact = rigcoder::observe::ProviderRetry::from_action(&retry.action)
        .unwrap()
        .unwrap();
    assert_eq!(fact.attempt, 1);
    assert_eq!(cell.log().records.len(), 2);
    assert!(cell.log().records[0].outcome.is_err());
    assert!(cell.log().records[1].outcome.is_ok());
}

use rig::observe::HostAction as _;

#[test]
fn rate_limited_then_answered() {
    let derived = with_failure_first(
        "text_unary",
        "rate_limited_unary",
        429,
        "RESOURCE_EXHAUSTED",
        "Quota exceeded for quota metric 'Generate Content API requests per minute'",
    );
    run(
        MATRIX,
        "rate_limited_unary",
        Config {
            retries: 3,
            source: Source::derived(derived, "observe_turns", "text_unary"),
            ..Config::unary()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            let first = cell
                .find(|a| {
                    matches!(
                        a,
                        Action::Landed {
                            outcome: OutcomeSummary::Err { .. }
                        }
                    )
                })
                .unwrap();
            let Action::Landed {
                outcome: OutcomeSummary::Err { reason, retryable },
            } = first.action
            else {
                unreachable!()
            };
            assert!(retryable, "{reason:?}");
            retried_then_settled(cell, "http");
            let Err(report) = &cell.log().records[0].outcome else {
                panic!()
            };
            assert_eq!(report.http_status, Some(429));
        },
    );
}

#[test]
fn server_error_then_answered() {
    let derived = with_failure_first(
        "text_stream",
        "server_error_stream",
        503,
        "UNAVAILABLE",
        "The model is overloaded. Please try again later.",
    );
    run(
        MATRIX,
        "server_error_stream",
        Config {
            retries: 3,
            source: Source::derived(derived, "observe_turns", "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            let first = cell
                .find(|a| {
                    matches!(
                        a,
                        Action::Landed {
                            outcome: OutcomeSummary::Err { .. }
                        }
                    )
                })
                .unwrap();
            let Action::Landed {
                outcome: OutcomeSummary::Err { reason, retryable },
            } = first.action
            else {
                unreachable!()
            };
            assert!(retryable, "{reason:?}");
            retried_then_settled(cell, "http");
            let Err(report) = &cell.log().records[0].outcome else {
                panic!()
            };
            assert_eq!(report.http_status, Some(503));
        },
    );
}

/// Ask Gemini to block: the strictest thresholds on every category, and a
/// prompt that should trip at least a low-probability rating. If the live
/// API answers instead, the cell records that and says so.
#[test]
fn a_blocked_prompt() {
    pair(
        "blocked_prompt",
        |stream| Config {
            retries: 3,
            additional_params: Some(serde_json::json!({
                "safetySettings": [
                    {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_LOW_AND_ABOVE"},
                    {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_LOW_AND_ABOVE"},
                    {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_LOW_AND_ABOVE"},
                    {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_LOW_AND_ABOVE"}
                ]
            })),
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit(
                "Write a vivid, insulting rant mocking a coworker's intelligence, with profanity.",
            );
            cell.drive();
            let facts = cell.facts();
            eprintln!("[{}/{}] ending: {}", MATRIX, cell.name, cell.ending());
            assert_eq!(
                cell.count("rigcoder/provider_retry"),
                0,
                "a verdict is not transient: {facts:?}"
            );
            assert_eq!(
                cell.count("stream_truncated"),
                0,
                "a verdict is not a truncation: {facts:?}"
            );
            assert_eq!(cell.log().records.len(), 1, "one request");
            if cell.ending() == "settled" {
                eprintln!(
                    "[{}/{}] the live API answered instead of blocking; see the table",
                    MATRIX, cell.name
                );
            } else {
                assert!(
                    cell.ending().contains("blocked the prompt")
                        || cell.ending().contains("SAFETY")
                        || cell.ending().contains("block"),
                    "{}",
                    cell.ending()
                );
                let (_, kind, _) = ending_matches_landed(cell);
                assert_eq!(kind, "provider");
            }
        },
    );
}

fn frames_of(docs: &mut [serde_yaml::Value]) -> Vec<String> {
    sse_frames(body_of(&mut docs[0]))
}

#[test]
fn a_stream_cut_before_its_terminal_is_retried() {
    let source = cassette_path("observe_turns", "text_stream");
    let derived = derive(
        &source,
        &cassette_path(MATRIX, "stream_truncated_stream"),
        |docs| {
            let mut frames = frames_of(docs);
            assert!(frames.len() >= 2, "{frames:?}");
            frames.pop();
            let whole = docs[0].clone();
            *body_of(&mut docs[0]) = join_frames(&frames);
            docs.push(whole);
        },
    );
    run(
        MATRIX,
        "stream_truncated_stream",
        Config {
            retries: 3,
            source: Source::derived(derived, "observe_turns", "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            let facts = cell.facts();
            eprintln!("[{MATRIX}/stream_truncated_stream] facts: {facts:?}");
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert_eq!(cell.count("stream_truncated"), 1, "{facts:?}");
            assert_eq!(cell.count("rigcoder/provider_retry"), 1, "{facts:?}");
            let truncated = cell
                .find(|a| matches!(a, Action::StreamTruncated { .. }))
                .unwrap();
            assert_eq!(truncated.emitter.name, "rig-ecs/bus");
            let Action::StreamTruncated {
                delivered,
                tail,
                errors,
            } = truncated.action
            else {
                unreachable!()
            };
            assert!(delivered >= 1, "{delivered}");
            assert!(!tail.is_empty(), "the last frames explain the cut");
            assert!(
                errors.is_empty(),
                "a clean cut carries no error: {errors:?}"
            );
            retried_then_settled(cell, "response");
        },
    );
}

#[test]
fn an_error_frame_after_text() {
    let source = cassette_path("observe_turns", "text_stream");
    let derived = derive(
        &source,
        &cassette_path(MATRIX, "stream_error_frame_stream"),
        |docs| {
            let mut frames = frames_of(docs);
            frames.pop();
            frames.push(format!(
                "data: {}",
                gemini_error(500, "INTERNAL", "An internal error has occurred.")
            ));
            *body_of(&mut docs[0]) = join_frames(&frames);
        },
    );
    run(
        MATRIX,
        "stream_error_frame_stream",
        Config {
            source: Source::derived(derived, "observe_turns", "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            let facts = cell.facts();
            eprintln!(
                "[{MATRIX}/stream_error_frame_stream] facts: {facts:?} ending: {}",
                cell.ending()
            );
            assert_ne!(cell.ending(), "settled");
            let (code, kind, outcome) = ending_matches_landed(cell);
            eprintln!(
                "[{MATRIX}/stream_error_frame_stream] ended:{code} landed:{kind} {outcome:?}"
            );
            if let Some(t) = cell.find(|a| matches!(a, Action::StreamTruncated { .. })) {
                eprintln!(
                    "[{MATRIX}/stream_error_frame_stream] truncation: {}",
                    serde_json::to_string(&t.action).unwrap()
                );
            }
            let json = serde_json::to_string(&cell.trace()).unwrap();
            assert!(
                json.contains("internal error"),
                "the error text reaches the trace: {json}"
            );
        },
    );
}

#[test]
fn a_malformed_frame() {
    let source = cassette_path("observe_turns", "text_stream");
    let derived = derive(
        &source,
        &cassette_path(MATRIX, "malformed_frame_stream"),
        |docs| {
            let mut frames = frames_of(docs);
            frames[0] = "data: {\"candidates\": [{\"content\": ".to_owned();
            *body_of(&mut docs[0]) = join_frames(&frames);
        },
    );
    run(
        MATRIX,
        "malformed_frame_stream",
        Config {
            source: Source::derived(derived, "observe_turns", "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            let facts = cell.facts();
            eprintln!(
                "[{MATRIX}/malformed_frame_stream] facts: {facts:?} ending: {}",
                cell.ending()
            );
            assert_ne!(cell.ending(), "settled");
            let (code, kind, outcome) = ending_matches_landed(cell);
            eprintln!("[{MATRIX}/malformed_frame_stream] ended:{code} landed:{kind} {outcome:?}");
            if let Some(t) = cell.find(|a| matches!(a, Action::StreamTruncated { .. })) {
                eprintln!(
                    "[{MATRIX}/malformed_frame_stream] truncation: {}",
                    serde_json::to_string(&t.action).unwrap()
                );
            }
            assert_eq!(
                cell.count("rigcoder/provider_retry"),
                0,
                "a parse failure is not retried: {facts:?}"
            );
        },
    );
}

#[test]
fn a_transport_cut_mid_frame() {
    let source = cassette_path("observe_turns", "text_stream");
    let derived = derive(
        &source,
        &cassette_path(MATRIX, "transport_cut_stream"),
        |docs| {
            let mut frames = frames_of(docs);
            let last = frames.pop().unwrap();
            let cut: String = last.chars().take(last.chars().count() / 2).collect();
            let mut body = join_frames(&frames);
            body.push_str(&cut);
            *body_of(&mut docs[0]) = body;
        },
    );
    run(
        MATRIX,
        "transport_cut_stream",
        Config {
            source: Source::derived(derived, "observe_turns", "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            let facts = cell.facts();
            eprintln!(
                "[{MATRIX}/transport_cut_stream] facts: {facts:?} ending: {}",
                cell.ending()
            );
            assert_ne!(cell.ending(), "settled");
            let (code, kind, outcome) = ending_matches_landed(cell);
            eprintln!("[{MATRIX}/transport_cut_stream] ended:{code} landed:{kind} {outcome:?}");
            if let Some(t) = cell.find(|a| matches!(a, Action::StreamTruncated { .. })) {
                eprintln!(
                    "[{MATRIX}/transport_cut_stream] truncation: {}",
                    serde_json::to_string(&t.action).unwrap()
                );
            }
        },
    );
}
