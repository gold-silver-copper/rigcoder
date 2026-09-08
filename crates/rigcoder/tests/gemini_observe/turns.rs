//! Matrix A — turn shapes × delivery (`observe_turns`).
//!
//! Every cell is a unary/streamed parity pair recorded against the real
//! API; the semantic facts must agree between the pair (`semantic_facts`:
//! everything but a delivery-only truncation).
//!
//! | cell | dimension pinned | oracle | facts asserted | status |
//! |---|---|---|---|---|
//! | `text_{unary,stream}` | one text turn | settled, one request, answer says pong | `issued, landed, ended:settled`; all scoped to the run; `Landed` carries the effect id | recorded |
//! | `one_tool_{unary,stream}` | one bash call, auto approval, then text | file written, two requests | `held`, approval `prepared`/`approved`, `released`, then the tool `issued`: the hold is the gate's decision, no churn; tool subject keyed `tool:bash`, family Tool, effect id in the log | recorded |
//! | `batch_c1_{unary,stream}`, `batch_c4_{unary,stream}` | four calls, concurrency 1 / 4 | four ok results, two requests | holds ≥ 4, each released once, four approvals; concurrency 1 holds more | recorded |
//! | `two_tools_{unary,stream}` | bash, then read_file, then text | three requests, file content in answer | `Issued` subjects for the completions carry increasing `order`; tools carry their key | recorded |
//! | `invalid_tool_{unary,stream}` | a call to a function the agent was never given | run failed `UnknownToolCall` | `invalid_call` (name, resolution `fail`), `ended:unknown_tool_call` last | recorded |
//! | `thinking_{unary,stream}` | `includeThoughts` on (`gemini-2.5-flash`, which returns thought parts) | settled; the recorded request asks for thoughts and the recorded response carries ≥ 1 thought part; the record's outcome holds the reasoning | the trace carries no thought or reasoning text (payload policy); facts unchanged | recorded |
//! | `max_tokens_{unary,stream}` | `finishReason: MAX_TOKENS` under a six-token cap | what the recorded bytes say: no parts → `ended:provider` (empty response), any member → `ended:settled`; never a retry | as stated; no parity claim (Gemini's shape varies per recording) | recorded |
//! | `empty_candidate_unary` | `content: {}` (no parts), `MAX_TOKENS` | run failed on the empty response | `landed` Err `response`, not retryable, no `provider_retry` | derived from `max_tokens_unary` (content emptied) |
//! | `empty_member_stream` | one `"text": ""` part, `MAX_TOKENS` | settled with an empty answer | `issued, landed, ended:settled` | derived from `text_stream` (text emptied) |

use crate::support::*;
use rig::observe::Action;

const MATRIX: &str = "observe_turns";

#[test]
fn text_answer() {
    pair(MATRIX, "text", Config::delivery, |cell, _| {
        cell.submit("Reply with the single word: pong");
        cell.drive();
        assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
        assert!(
            cell.answer().to_lowercase().contains("pong"),
            "{}",
            cell.answer()
        );
        assert_eq!(cell.log().records.len(), 1);
        let facts = cell.facts();
        assert_eq!(facts, ["issued", "landed", "ended:settled"], "{facts:?}");
        let trace = cell.trace();
        assert!(trace.is_complete());
        assert!(
            trace
                .observations
                .iter()
                .all(|o| o.subject.scope.as_deref() == Some("rigcoder/run/1")),
            "{trace:?}"
        );
        let landed = cell.find(|a| matches!(a, Action::Landed { .. })).unwrap();
        assert!(landed.subject.effect.is_some(), "{landed:?}");
        assert_eq!(landed.emitter.name, "rig-ecs/bus");
    });
}

#[test]
fn one_tool_call() {
    pair(MATRIX, "one_tool", Config::delivery, |cell, _| {
        cell.submit("Run this command: printf hi > out.txt");
        cell.drive();
        assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
        assert_eq!(
            std::fs::read_to_string(cell.dir.join("out.txt")).unwrap(),
            "hi"
        );
        assert_eq!(cell.log().records.len(), 3, "two completions and one tool");
        let facts = cell.facts();
        assert_eq!(
            facts,
            [
                "issued",
                "landed",
                "held",
                "rigcoder/approval:prepared",
                "rigcoder/approval:approved",
                "released",
                "issued",
                "landed",
                "issued",
                "landed",
                "ended:settled"
            ],
            "{facts:?}"
        );
        let trace = cell.trace();
        let tool = trace
            .observations
            .iter()
            .find(|o| {
                o.subject
                    .key
                    .as_ref()
                    .is_some_and(|k| k.as_str() == "tool:bash")
            })
            .expect("a tool fact keyed tool:bash");
        assert_eq!(
            tool.subject.family,
            Some(rig::effect::EffectFamily::Tool),
            "{tool:?}"
        );
        let log = cell.log();
        let tool_landed = trace
            .observations
            .iter()
            .find(|o| {
                matches!(o.action, Action::Landed { .. })
                    && o.subject
                        .key
                        .as_ref()
                        .is_some_and(|k| k.as_str() == "tool:bash")
            })
            .unwrap();
        let id = tool_landed.subject.effect.unwrap();
        assert!(
            log.records.iter().any(|r| r.id == id),
            "the tool's effect id {id:?} is a record: {:?}",
            log.records.iter().map(|r| r.id).collect::<Vec<_>>()
        );
    });
}

#[test]
fn a_batch_under_concurrency_one_and_four() {
    HOLDS.lock().unwrap().clear();
    for concurrency in [1usize, 4] {
        let holds = std::sync::Mutex::new(Vec::new());
        pair(
            MATRIX,
            &format!("batch_c{concurrency}"),
            if concurrency == 1 {
                |stream| Config {
                    concurrency: Some(1),
                    ..Config::delivery(stream)
                }
            } else {
                |stream| Config {
                    concurrency: Some(4),
                    ..Config::delivery(stream)
                }
            },
            |cell, _| {
                cell.submit(
                    "Run these four commands, each as its own bash call, all in this one reply: echo 1 ; echo 2 ; echo 3 ; echo 4 (four separate calls, one command each)",
                );
                cell.drive();
                assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
                assert_eq!(cell.tool_results(), 4, "{:?}", cell.events());
                let held = cell.count("held");
                assert!(held >= 4, "{:?}", cell.facts());
                assert_eq!(cell.count("released"), held, "every hold is released once");
                assert_eq!(cell.count("rigcoder/approval:approved"), 4);
                holds.lock().unwrap().push(held);
            },
        );
        let holds = holds.into_inner().unwrap();
        eprintln!("[{MATRIX}] concurrency {concurrency}: holds {holds:?}");
        HOLDS.lock().unwrap().push((concurrency, holds));
    }
    let holds = HOLDS.lock().unwrap();
    let one = holds
        .iter()
        .find(|(c, _)| *c == 1)
        .map(|(_, h)| h.clone())
        .unwrap();
    let four = holds
        .iter()
        .find(|(c, _)| *c == 4)
        .map(|(_, h)| h.clone())
        .unwrap();
    assert!(
        one[0] > four[0] && one[1] > four[1],
        "concurrency 1 holds more than 4: {one:?} vs {four:?}"
    );
}

static HOLDS: std::sync::Mutex<Vec<(usize, Vec<usize>)>> = std::sync::Mutex::new(Vec::new());

#[test]
fn two_tools_in_sequence() {
    pair(MATRIX, "two_tools", Config::delivery, |cell, _| {
        cell.submit(
            "First run this command: printf 'alpha beta' > note.txt . Then, after its result, read the file note.txt with read_file. Then reply with the file's contents.",
        );
        cell.drive();
        assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
        assert!(cell.answer().contains("alpha"), "{}", cell.answer());
        assert_eq!(cell.log().records.len(), 5, "three completions, two tools");
        let trace = cell.trace();
        let completions: Vec<_> = trace
            .observations
            .iter()
            .filter(|o| {
                matches!(o.action, Action::Issued)
                    && o.subject
                        .key
                        .as_ref()
                        .is_some_and(|k| k.as_str() == rigcoder::model::MODEL_KEY)
            })
            .collect();
        assert_eq!(completions.len(), 3, "{trace:?}");
        let orders: Vec<_> = completions.iter().map(|o| o.subject.order).collect();
        assert!(
            orders.windows(2).all(|w| w[0] < w[1]),
            "completion order increases: {orders:?}"
        );
        let tools: Vec<_> = trace
            .observations
            .iter()
            .filter(|o| matches!(o.action, Action::Issued))
            .filter_map(|o| o.subject.key.as_ref().map(|k| k.as_str().to_owned()))
            .filter(|k| k.starts_with("tool:"))
            .collect();
        assert_eq!(tools, ["tool:bash", "tool:read_file"], "{trace:?}");
    });
}

#[test]
fn an_invalid_tool_call() {
    pair(
        MATRIX,
        "invalid_tool",
        |stream| Config {
            prompt: "You are a test agent. You have a function named teleport that takes no arguments. Call it whenever the user asks, without any other text.",
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Call the teleport function now.");
            cell.drive();
            assert!(
                cell.ending().contains("UnknownToolCall"),
                "{}: {:?}",
                cell.ending(),
                cell.events()
            );
            let facts = cell.facts();
            assert!(facts.contains(&"invalid_call".to_owned()), "{facts:?}");
            assert_eq!(
                facts.last().map(String::as_str),
                Some("ended:unknown_tool_call")
            );
            let invalid = cell
                .find(|a| matches!(a, Action::InvalidCall { .. }))
                .unwrap();
            let Action::InvalidCall { name, resolution } = invalid.action else {
                unreachable!()
            };
            assert_eq!(name, "teleport");
            assert_eq!(resolution.code, "fail");
        },
    );
}

/// `includeThoughts` on a model that returns thought parts
/// (`gemini-2.5-flash`; `gemini-3.8-flash` counts thoughts in usage but
/// returned none on 2026-09-08, which would leave the payload-policy check
/// vacuous).
#[test]
fn thinking_enabled() {
    pair(
        MATRIX,
        "thinking",
        |stream| Config {
            model: "gemini-2.5-flash",
            additional_params: Some(serde_json::json!({
                "generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 512}}
            })),
            ..Config::delivery(stream)
        },
        |cell, stream| {
            cell.submit("What is 17 + 25? Think it through, then reply with just the number.");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert!(cell.answer().contains("42"), "{}", cell.answer());
            let facts = cell.facts();
            assert_eq!(facts, ["issued", "landed", "ended:settled"], "{facts:?}");
            if !cell.recording() {
                // The wire: the request asked, the response carried thoughts.
                let scenario = format!("{MATRIX}/{}", cell.name);
                let request =
                    rig_cassette::recorded_json_request(&cassette_root(), PROVIDER, &scenario);
                assert_eq!(
                    request["generationConfig"]["thinkingConfig"]["includeThoughts"], true,
                    "{request}"
                );
                let thought_parts: usize = if stream {
                    rig_cassette::recorded_sse_json_frames(&cassette_root(), PROVIDER, &scenario)
                        .iter()
                        .flat_map(|f| {
                            f["candidates"][0]["content"]["parts"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default()
                        })
                        .filter(|p| p["thought"] == true)
                        .count()
                } else {
                    rig_cassette::recorded_json_response(&cassette_root(), PROVIDER, &scenario)["candidates"][0]["content"]["parts"]
                        .as_array()
                        .map_or(0, |parts| parts.iter().filter(|p| p["thought"] == true).count())
                };
                assert!(thought_parts >= 1, "the recording carries thought parts");
            }
            // The record holds the reasoning; the trace only summarises the
            // outcome and never carries reasoning text.
            let log = cell.log();
            assert!(
                serde_json::to_string(&log.records[0].outcome)
                    .unwrap()
                    .contains("easoning"),
                "the record keeps the thoughts"
            );
            let json = serde_json::to_string(&cell.trace()).unwrap();
            assert!(
                !json.contains("thought") && !json.contains("easoning"),
                "{json}"
            );
        },
    );
}

/// A `MAX_TOKENS` finish under a six-token cap. Gemini's shape for it is
/// not stable across recordings (seen on 2026-09-08: `content: {}` with no
/// parts; one empty text part with a thought signature; two tokens of
/// text), so the recorded pair asserts what its bytes say, and the two
/// empty shapes are pinned by derived fixtures below. Rig rejects an empty
/// part *list* (`EMPTY_RESPONSE_ERROR`) and accepts an empty *member*.
#[test]
fn max_tokens_cut() {
    for stream in [false, true] {
        let name = if stream {
            "max_tokens_stream"
        } else {
            "max_tokens_unary"
        };
        run(
            MATRIX,
            name,
            Config {
                max_tokens: 6,
                additional_params: Some(serde_json::json!({
                    "generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}
                })),
                ..Config::delivery(stream)
            },
            |cell| {
                cell.submit("Write a paragraph of at least one hundred words about rivers.");
                cell.drive();
                let facts = cell.facts();
                eprintln!(
                    "[{}/{}] facts: {facts:?} ending: {}",
                    MATRIX,
                    cell.name,
                    cell.ending()
                );
                assert_eq!(
                    cell.count("rigcoder/provider_retry"),
                    0,
                    "a length cut is not transient"
                );
                if !cell.recording() {
                    let scenario = format!("{MATRIX}/{}", cell.name);
                    let (finish, parts) = if stream {
                        let frames = rig_cassette::recorded_sse_json_frames(
                            &cassette_root(),
                            PROVIDER,
                            &scenario,
                        );
                        let finish = frames.iter().rev().find_map(|f| {
                            f["candidates"][0]["finishReason"]
                                .as_str()
                                .map(str::to_owned)
                        });
                        let parts = frames
                            .iter()
                            .map(|f| {
                                f["candidates"][0]["content"]["parts"]
                                    .as_array()
                                    .map_or(0, Vec::len)
                            })
                            .sum::<usize>();
                        (finish, parts)
                    } else {
                        let wire = rig_cassette::recorded_json_response(
                            &cassette_root(),
                            PROVIDER,
                            &scenario,
                        );
                        (
                            wire["candidates"][0]["finishReason"]
                                .as_str()
                                .map(str::to_owned),
                            wire["candidates"][0]["content"]["parts"]
                                .as_array()
                                .map_or(0, Vec::len),
                        )
                    };
                    assert_eq!(finish.as_deref(), Some("MAX_TOKENS"));
                    if parts == 0 {
                        assert_eq!(
                            facts,
                            ["issued", "landed", "ended:provider"],
                            "no parts: rejected as empty"
                        );
                        assert!(
                            cell.ending().contains("no message or tool call"),
                            "{}",
                            cell.ending()
                        );
                    } else {
                        assert_eq!(
                            facts,
                            ["issued", "landed", "ended:settled"],
                            "a member, empty or not, is an answer"
                        );
                    }
                }
            },
        );
    }
}

/// The unary empty shape: `content: {}`, no parts, `MAX_TOKENS`. Derived
/// from `max_tokens_unary` by emptying the candidate's content.
#[test]
fn an_empty_candidate_is_rejected_unary() {
    let source = cassette_path(MATRIX, "max_tokens_unary");
    let derived = derive(
        &source,
        &cassette_path(MATRIX, "empty_candidate_unary"),
        |docs| {
            let body = body_of(&mut docs[0]);
            let mut json: serde_json::Value = serde_json::from_str(body).unwrap();
            json["candidates"][0]["content"] = serde_json::json!({});
            *body = json.to_string();
        },
    );
    run(
        MATRIX,
        "empty_candidate_unary",
        Config {
            max_tokens: 6,
            retries: 3,
            additional_params: Some(serde_json::json!({
                "generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}
            })),
            source: Source::derived(derived, MATRIX, "max_tokens_unary"),
            ..Config::unary()
        },
        |cell| {
            cell.submit("Write a paragraph of at least one hundred words about rivers.");
            cell.drive();
            assert!(
                cell.ending().contains("no message or tool call"),
                "{}",
                cell.ending()
            );
            let facts = cell.facts();
            assert_eq!(facts, ["issued", "landed", "ended:provider"], "{facts:?}");
            let Action::Landed { outcome } = cell
                .find(|a| matches!(a, Action::Landed { .. }))
                .unwrap()
                .action
            else {
                unreachable!()
            };
            let rig::observe::OutcomeSummary::Err { reason, retryable } = outcome else {
                panic!("{outcome:?}")
            };
            assert_eq!(reason.code, rig::error::ErrorKind::Response.code());
            assert!(!retryable);
            assert_eq!(
                cell.count("rigcoder/provider_retry"),
                0,
                "not transient, even with retries allowed"
            );
            // The usage of the rejected attempt is lost with it (a follow-up
            // for the provider decode, noted in the ledger).
        },
    );
}

/// The streamed empty shape: text parts with `"text": ""` (one per frame),
/// `MAX_TOKENS`. Derived from `text_stream` by emptying its text.
#[test]
fn an_empty_member_settles_stream() {
    let source = cassette_path(MATRIX, "text_stream");
    let derived = derive(
        &source,
        &cassette_path(MATRIX, "empty_member_stream"),
        |docs| {
            let body = body_of(&mut docs[0]);
            let frames: Vec<String> = sse_frames(body)
                .into_iter()
                .map(|frame| {
                    let mut json: serde_json::Value =
                        serde_json::from_str(frame.trim_start_matches("data: ")).unwrap();
                    if let Some(parts) = json["candidates"][0]["content"]["parts"].as_array_mut() {
                        for part in parts {
                            if part.get("text").is_some() {
                                part["text"] = serde_json::Value::String(String::new());
                            }
                        }
                    }
                    if json["candidates"][0].get("finishReason").is_some() {
                        json["candidates"][0]["finishReason"] =
                            serde_json::Value::String("MAX_TOKENS".into());
                    }
                    format!("data: {json}")
                })
                .collect();
            *body = join_frames(&frames);
        },
    );
    run(
        MATRIX,
        "empty_member_stream",
        Config {
            source: Source::derived(derived, MATRIX, "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert_eq!(cell.answer(), "");
            assert_eq!(cell.facts(), ["issued", "landed", "ended:settled"]);
        },
    );
}
