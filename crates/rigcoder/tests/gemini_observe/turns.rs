//! Matrix A — turn shapes × delivery (`observe_turns`).
//!
//! Every cell is a unary/streamed parity pair recorded against the real
//! API; lifecycle facts must agree between the pair. Adapter frame boundaries
//! differ by wire and remain asserted in each complete evidence packet.
//!
//! | cell | dimension pinned | oracle | facts asserted | status |
//! |---|---|---|---|---|
//! | `text_{unary,stream}` | one text turn | settled, one request, answer says pong | `issued, landed, ended:settled`; all scoped to the run; `Landed` carries the effect id | recorded |
//! | `one_tool_{unary,stream}` | one bash call, auto approval, then text | file written, two requests | `held`, approval `prepared`/`approved`, `released`, then the tool `issued`: the hold is the gate's decision, no churn; tool subject keyed `tool:bash`, family Tool, effect id in the log | recorded |
//! | `calls4_c1_{unary,stream}`, `calls4_c4_{unary,stream}` | four calls, concurrency 1 / 4 | four ok results, two requests | four approval owners; three batch owners under 1, none under 4; every release matches its owner by scope/order and no tool issues while held | recorded; evidence packet remains volatile under the existing comparison policy |
//! | `two_tools_{unary,stream}` | bash, then read_file, then text | three requests, file content in answer | `Issued` subjects for the completions carry increasing `order`; tools carry their key | recorded |
//! | `invalid_tool_{unary,stream}` | a call to a function the agent was never given | run failed `UnknownToolCall` | `ended:unknown_tool_call` and structured failure identifying the invalid tool | recorded |
//! | `thinking_{unary,stream}` | `includeThoughts` on (`gemini-2.5-flash`, which returns thought parts) | settled; the recorded request asks for thoughts and the recorded response carries ≥ 1 thought part; the record's outcome holds the reasoning | the trace carries no thought or reasoning text (payload policy); facts unchanged | recorded |
//! | `max_tokens_{unary,stream}` | `finishReason: MAX_TOKENS` under a six-token cap | both fixed recordings settle with a nonempty short answer; no retry | emitted adapter verdict is MAX_TOKENS; lifecycle ends settled | recorded |
//! | `empty_candidate_unary` | `content: {}` (no parts), `MAX_TOKENS` | run failed on the empty response | adapter HTTP 200, usage 802 input/2 output/804 total preserved before response-error closure; `landed` Err, no retry | derived from `max_tokens_unary` (content emptied) |
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
        let facts = cell.lifecycle_facts();
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
        let facts = cell.lifecycle_facts();
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
        for stream in [false, true] {
            run(
                MATRIX,
                &format!(
                    "calls4_c{concurrency}_{}",
                    if stream { "stream" } else { "unary" }
                ),
                // Retain the existing volatile packet policy. Owner transitions
                // and within-trace joins are asserted on every replay; cross-run
                // dispatch identity and semantic equality remain deferred.
                Config {
                    concurrency: Some(concurrency),
                    volatile: true,
                    ..Config::delivery(stream)
                },
                |cell| {
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
                    let trace = cell.trace();
                    let completion_lifecycle: Vec<_> = trace
                        .observations
                        .iter()
                        .filter(|observation| {
                            observation.subject.family != Some(rig::effect::EffectFamily::Tool)
                        })
                        .map(|observation| fact(&observation.action, observation.stage))
                        .filter(|label| label != "adapter" && label != "stream_truncated")
                        .collect();
                    assert_eq!(
                        completion_lifecycle,
                        ["issued", "landed", "issued", "landed", "ended:settled"]
                    );
                    let mut active = std::collections::BTreeSet::new();
                    let mut batch_holds = 0;
                    let mut approval_holds = 0;
                    for fact in &trace.observations {
                        if matches!(fact.action, Action::Held { .. } | Action::Released) {
                            assert!(fact.subject.scope.is_some());
                            let order = fact.subject.order.expect("held tool dispatch order");
                            let owner = fact.emitter.name.as_str();
                            assert!(matches!(owner, "rig-ecs/batch" | "rigcoder/approval"));
                            let key = (fact.subject.scope.as_deref(), order, owner);
                            if matches!(fact.action, Action::Held { .. }) {
                                assert!(active.insert(key), "duplicate acquisition: {fact:?}");
                                if owner == "rig-ecs/batch" {
                                    batch_holds += 1;
                                } else {
                                    approval_holds += 1;
                                }
                            } else {
                                assert!(active.remove(&key), "release without owner: {fact:?}");
                            }
                        }
                        if matches!(fact.action, Action::Issued) {
                            assert!(
                                !active.iter().any(|(scope, order, _)| *scope
                                    == fact.subject.scope.as_deref()
                                    && Some(*order) == fact.subject.order),
                                "issued while held: {fact:?}"
                            );
                        }
                    }
                    assert!(active.is_empty());
                    assert_eq!(approval_holds, 4);
                    assert_eq!(batch_holds, if concurrency == 1 { 3 } else { 0 });
                    // Independent calls may interleave. Pin each call's causal
                    // chain and the actual concurrency bound, not a global order.
                    let mut chains = std::collections::BTreeMap::<_, Vec<String>>::new();
                    let mut in_flight = std::collections::BTreeSet::new();
                    for observation in &trace.observations {
                        if observation.subject.family != Some(rig::effect::EffectFamily::Tool) {
                            continue;
                        }
                        let label = fact(&observation.action, observation.stage);
                        if matches!(
                            label.as_str(),
                            "rigcoder/approval:prepared"
                                | "rigcoder/approval:approved"
                                | "issued"
                                | "landed"
                        ) || (observation.emitter.name == "rigcoder/approval"
                            && matches!(label.as_str(), "held" | "released"))
                        {
                            chains
                                .entry((
                                    observation.subject.scope.clone(),
                                    observation.subject.order.expect("tool order"),
                                ))
                                .or_default()
                                .push(label);
                        }
                        if matches!(observation.action, Action::Issued) {
                            assert!(
                                in_flight
                                    .insert(observation.subject.effect.expect("issued effect"))
                            );
                            assert!(in_flight.len() <= concurrency);
                        } else if matches!(observation.action, Action::Landed { .. }) {
                            assert!(
                                in_flight
                                    .remove(&observation.subject.effect.expect("landed effect"))
                            );
                        }
                    }
                    assert!(in_flight.is_empty());
                    assert_eq!(chains.len(), 4);
                    for chain in chains.values() {
                        assert_eq!(
                            chain,
                            &[
                                "held",
                                "rigcoder/approval:prepared",
                                "rigcoder/approval:approved",
                                "released",
                                "issued",
                                "landed"
                            ]
                        );
                    }
                    holds.lock().unwrap().push(held);
                },
            );
        }
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
    // Under 4 every hold is the gate's; under 1 the batch also holds three calls.
    assert!(
        one[0] >= four[0] && one[1] >= four[1],
        "concurrency 1 holds at least as much as 4: {one:?} vs {four:?}"
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
        // The frozen stream packet records full delivery before policy. The
        // gated observe_matrix regression covers failure before producer EOF.
        |stream| Config {
            complete_delivery_before_policy: stream,
            prompt: "You are a test agent. You have a function named teleport that takes no arguments. Call it whenever the user asks, without any other text.",
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Call the teleport function now.");
            cell.drive();
            assert!(
                cell.failure().kind == "unknown_tool_call",
                "{}: {:?}",
                cell.ending(),
                cell.events()
            );
            let facts = cell.facts();
            assert_eq!(
                &facts[facts.len() - 2..],
                ["ended:unknown_tool_call", "rigcoder/failure"]
            );
            assert!(cell.failure().message.contains("teleport"));
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
            let facts = cell.lifecycle_facts();
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
            let Ok(rig::effect::Outcome::Completion(response)) = &log.records[0].outcome else {
                panic!("a completed model answer");
            };
            let thoughts: Vec<_> = response
                .choice
                .iter()
                .filter_map(|content| {
                    let rig::message::AssistantContent::Reasoning(reasoning) = content else {
                        return None;
                    };
                    Some(&reasoning.content)
                })
                .flatten()
                .filter_map(|content| match content {
                    rig::message::ReasoningContent::Text { text, .. } if !text.is_empty() => {
                        Some(text)
                    }
                    _ => None,
                })
                .collect();
            assert!(!thoughts.is_empty(), "the record keeps the thoughts");
            let json = serde_json::to_string(&cell.trace()).unwrap();
            for thought in thoughts {
                assert!(
                    !json.contains(&serde_json::to_string(thought).unwrap()),
                    "reasoning text must not enter the trace"
                );
            }
        },
    );
}

/// A `MAX_TOKENS` finish under a six-token cap. Gemini's shape for it is
/// not stable across recordings (seen on 2026-09-08: `content: {}` with no
/// parts; one empty text part with a thought signature; two tokens of
/// text). This pair pins the current recordings and reads the verdict from
/// adapter facts; the two empty shapes are pinned by derived fixtures below. Rig rejects an empty
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
                let facts = cell.lifecycle_facts();
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
                let trace = cell.trace();
                assert!(trace.observations.iter().any(|fact| matches!(&fact.action,
                    Action::Adapter { observation } if matches!(&observation.event,
                        rig::observe::AdapterEvent::Provider { verdict }
                            if verdict.finish_reason.as_deref() == Some("MAX_TOKENS")))));
                // Both fixed recordings contain the short answer "Rivers are".
                // The derived cells below pin rejection of an empty part list.
                assert_eq!(cell.ending(), "settled");
                assert_eq!(facts, ["issued", "landed", "ended:settled"]);
                assert!(!cell.answer().is_empty());
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
            assert!(cell.failure().kind == "response", "{}", cell.ending());
            let facts = cell.facts();
            assert_eq!(
                facts,
                [
                    "issued",
                    "adapter",
                    "adapter",
                    "adapter",
                    "adapter",
                    "adapter",
                    "landed",
                    "ended:provider",
                    "rigcoder/failure"
                ],
                "{facts:?}"
            );
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
            let trace = cell.trace();
            let adapter: Vec<_> = trace
                .observations
                .iter()
                .filter_map(|o| {
                    let Action::Adapter { observation } = &o.action else {
                        return None;
                    };
                    assert_eq!(o.subject.effect, Some(cell.log().records[0].id));
                    assert_eq!(observation.attempt, Some(1));
                    Some(observation)
                })
                .collect();
            assert_eq!(adapter.len(), 5);
            assert!(adapter.iter().all(|f| f.operation == adapter[0].operation));
            assert_eq!(
                adapter[1].event,
                rig::observe::AdapterEvent::Response { status: 200 }
            );
            assert_eq!(
                adapter[2].event,
                rig::observe::AdapterEvent::Usage {
                    usage: rig::observe::AdapterUsage {
                        input_tokens: Some(802),
                        output_tokens: Some(2),
                        total_tokens: Some(804),
                        ..rig::observe::AdapterUsage::default()
                    }
                }
            );
            assert_eq!(
                adapter[3].event,
                rig::observe::AdapterEvent::Provider {
                    verdict: rig::observe::AdapterVerdict {
                        finish_reason: Some("MAX_TOKENS".into()),
                        model: Some("gemini-3.8-flash".into()),
                        ..rig::observe::AdapterVerdict::default()
                    }
                }
            );
            assert_eq!(
                adapter[4].event,
                rig::observe::AdapterEvent::Finished {
                    ending: rig::observe::AdapterEnding::Error {
                        boundary: rig::observe::AdapterErrorBoundary::Decode,
                        kind: "response".into(),
                        status: None,
                        retryable: false,
                    }
                }
            );
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
            assert_eq!(
                cell.lifecycle_facts(),
                ["issued", "landed", "ended:settled"]
            );
        },
    );
}
