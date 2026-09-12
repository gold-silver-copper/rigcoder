//! Adapter boundary facts emitted by Rig during the real product session.
//!
//! | cell | oracle | adapter contract | provenance |
//! | --- | --- | --- | --- |
//! | `unary_http_boundary` | settled, pong, one effect | one send/status/closure, joined to the bus effect and scope | replay of `observe_turns/text_unary` |
//! | `stream_http_boundary` | settled, pong, one effect | HTTP 200 and terminal closure | replay of `observe_turns/text_stream` |
//! | `partial_frame_boundary` | failed, one effect | partial byte count and complete-frame count, distinct from clean EOF | replay of `observe_failures/transport_cut_stream`, derived by cutting the final frame of `observe_turns/text_stream` halfway through its JSON |
//! | `terminal_then_partial_frame` | settled, pong, one effect | terminal closure does not hide seven trailing bytes at transport EOF | derived from `observe_turns/text_stream` by appending `data: {` after its terminal frame |
//! | `blocked_prompt_boundary` | provider failure, no retry | block verdict and usage survive rejection | derived from `observe_turns/text_unary` by replacing its body with a SAFETY prompt-feedback response |
//! | `retry_headers_unary` | retry then settled | HTTP 429 envelope and retry header are distinct from the successful response | derived from `observe_failures/rate_limited_unary` by adding retry-after and a scrubbed request ID to the failed response |
//! | `optional_response_id_unary` | both variants rejected identically | ID-only metadata survives closure without adding semantic events; Rig and packet comparisons agree | derived pair from `observe_turns/text_unary`, replacing the body with empty candidates, with and without responseId |
//! | `optional_response_id_stream` | both variants end at EOF identically | actual ID-only frame insertion preserves EOF position and Rig/packet comparison | derived pair from `observe_turns/text_stream`, replacing body with one content frame, optionally surrounded by ID-only frames |
//! | `tool_result_starts_a_new_operation` | one tool then settled | changed completion after tool result does not inherit the first completion's retry identity | replay of `observe_turns/one_tool_unary` |
//! | `retry_exhaustion_{unary,stream}` | two failures, one retry, then failed | one operation, two host/send ordinals and separate failed-attempt usage; retryability survives budget exhaustion | derived by repeating the first error of `rate_limited_unary` / `server_error_stream`, adding distinct usage counts |
//! | `retry_observation_{off,on}` | same retry and answer | observation preserves transcript and semantic effect records | replay of `observe_failures/rate_limited_unary` |
//! | `invalid_stream_content_type` | one failed send, no retry | HTTP 200 preserved; typed decode boundary despite generic provider error report | derived from `observe_turns/text_stream` by replacing only the response Content-Type with text/plain |
//! | `direct_{unary,stream,failure}` | CompletionModel without dispatch | same adapter events/send ordinals as bus execution, explicit direct operation, no bus lifecycle facts | live-origin text unary/stream replays; derived unauthorized unary replay (401/status-body transformation from text unary) |

use crate::support::*;
use rig::observe::{Action, AdapterEnding, AdapterEvent, AdapterUsage, AdapterVerdict};

#[test]
fn direct_completion_matches_bus_adapter_facts() {
    for (name, matrix, source, streamed, failed) in [
        ("direct_unary", "observe_turns", "text_unary", false, false),
        ("direct_stream", "observe_turns", "text_stream", true, false),
        (
            "direct_failure",
            "observe_failures",
            "unauthorized_unary",
            false,
            true,
        ),
    ] {
        let packet = fixture_root()
            .join("evidence/gemini")
            .join(matrix)
            .join(source);
        let log: rigcoder::EffectLog =
            serde_json::from_str(&std::fs::read_to_string(packet.join("effects.json")).unwrap())
                .unwrap();
        let rig::effect::EffectKind::Completion { request, .. } = log.records[0].kind.clone()
        else {
            panic!("source begins with completion")
        };
        let baseline: rigcoder::observe::ObservationArtifact = serde_json::from_str(
            &std::fs::read_to_string(packet.join("observations.json")).unwrap(),
        )
        .unwrap();
        let facts = |trace: &rig::observe::ObservationTrace| {
            trace
                .observations
                .iter()
                .filter_map(|observation| {
                    if let Action::Adapter { observation } = &observation.action {
                        Some((observation.attempt, observation.event.clone()))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        run(
            "observe_wire",
            name,
            Config {
                source: if failed {
                    Source::derived(cassette_path(matrix, source), "observe_turns", "text_unary")
                } else {
                    Source::of(matrix, source)
                },
                ..Config::delivery(streamed)
            },
            |cell| {
                let answer = cell.direct_completion(request.clone(), streamed);
                assert_eq!(answer.is_err(), failed, "{answer:?}");
                if let Ok(answer) = answer {
                    let Ok(rig::effect::Outcome::Completion(expected)) = &log.records[0].outcome
                    else {
                        panic!("source completion succeeded")
                    };
                    assert_eq!(answer, expected.choice);
                }
                rigcoder::observe::finalize(cell.app.world());
                assert_eq!(
                    rigcoder::observe::artifact(cell.app.world())
                        .unwrap()
                        .recording_provenance,
                    Some(if failed {
                        rigcoder::observe::RecordingProvenance::Derived
                    } else {
                        rigcoder::observe::RecordingProvenance::Live
                    })
                );
                let trace = cell.trace();
                assert_eq!(facts(&trace), facts(&baseline.trace));
                assert!(!trace.observations.is_empty());
                for fact in &trace.observations {
                    let Action::Adapter { observation } = &fact.action else {
                        panic!("direct call emitted a bus fact: {fact:?}")
                    };
                    assert_eq!(observation.operation, "direct/1");
                    assert_eq!(observation.attempt, Some(1));
                    assert_eq!(fact.subject.scope.as_deref(), Some("direct"));
                    assert!(fact.subject.effect.is_none());
                }
                assert!(
                    cell.log().records.is_empty(),
                    "direct calls do not create effect records"
                );
            },
        );
    }
}

#[test]
fn invalid_stream_content_type() {
    let source = derive(
        &cassette_path("observe_turns", "text_stream"),
        &cassette_path("observe_wire", "invalid_stream_content_type"),
        |docs| {
            let headers = docs[0]["then"]["header"].as_sequence_mut().unwrap();
            let content_type = headers
                .iter_mut()
                .find(|header| {
                    header["name"]
                        .as_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case("content-type"))
                })
                .unwrap();
            content_type["value"] = serde_yaml::Value::String("text/plain".into());
        },
    );
    run(
        "observe_wire",
        "invalid_stream_content_type",
        Config {
            source: Source::derived(source, "observe_turns", "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            let failure = cell.failure();
            assert_eq!(failure.kind, "http");
            assert_eq!(failure.http_status, None);
            assert_eq!(failure.boundary, rigcoder::failure::FailureBoundary::Decode);
            let attempt = failure.adapter.as_ref().unwrap();
            assert_eq!(attempt.response_status, Some(200));
            assert!(matches!(
                attempt.ending,
                Some(AdapterEnding::Error {
                    boundary: rig::observe::AdapterErrorBoundary::Decode,
                    ..
                })
            ));
            assert_eq!(cell.count("rigcoder/provider_retry"), 0);
            assert_eq!(cell.log().records.len(), 1);
            rigcoder::observe::finalize(cell.app.world());
        },
    );
}

#[test]
fn observation_preserves_retry_decisions_and_effects() {
    let mut results = Vec::new();
    for enabled in [false, true] {
        run(
            "observe_wire",
            if enabled {
                "retry_observation_on"
            } else {
                "retry_observation_off"
            },
            Config {
                retries: 1,
                witness: enabled.then(WitnessConfig::default),
                source: Source::derived(
                    cassette_path("observe_failures", "rate_limited_unary"),
                    "observe_turns",
                    "text_unary",
                ),
                ..Config::unary()
            },
            |cell| {
                cell.submit("Reply with the single word: pong");
                cell.drive();
                assert_eq!(cell.ending(), "settled");
                assert_eq!(cell.log().records.len(), 2);
                if enabled {
                    assert_eq!(cell.count("rigcoder/provider_retry"), 1);
                    rigcoder::observe::finalize(cell.app.world());
                } else {
                    assert!(rigcoder::observations(cell.app.world()).is_none());
                }
                results.push((
                    normalized("effects.json", &serde_json::to_string(&cell.log()).unwrap()),
                    normalized(
                        "transcript.jsonl",
                        &serde_json::to_string(&cell.events()).unwrap(),
                    ),
                ));
            },
        );
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn retry_exhaustion_keeps_operation_and_failed_attempt_usage() {
    for (stream, source_name, original, name, status) in [
        (
            false,
            "rate_limited_unary",
            "text_unary",
            "retry_exhaustion_unary",
            429,
        ),
        (
            true,
            "server_error_stream",
            "text_stream",
            "retry_exhaustion_stream",
            503,
        ),
    ] {
        let source = derive(
            &cassette_path("observe_failures", source_name),
            &cassette_path("observe_wire", name),
            |docs| {
                let first = docs[0].clone();
                *docs = vec![first.clone(), first];
                for (index, doc) in docs.iter_mut().enumerate() {
                    let mut body: serde_json::Value = serde_json::from_str(body_of(doc)).unwrap();
                    body["usageMetadata"] = serde_json::json!({"promptTokenCount": 11 + index * 2, "totalTokenCount": 11 + index * 2});
                    *body_of(doc) = body.to_string();
                }
            },
        );
        run(
            "observe_wire",
            name,
            Config {
                retries: 1,
                witness: Some(WitnessConfig {
                    clock: true,
                    ..Default::default()
                }),
                source: Source::derived(source, "observe_turns", original),
                ..Config::delivery(stream)
            },
            |cell| {
                cell.submit("Reply with the single word: pong");
                cell.drive();
                assert_ne!(cell.ending(), "settled");
                assert_eq!(cell.count("rigcoder/provider_retry"), 1);
                assert_eq!(cell.log().records.len(), 2);
                rigcoder::observe::finalize(cell.app.world());
                let trace = cell.trace();
                let mut operation = None;
                for (index, record) in cell.log().records.iter().enumerate() {
                    let facts: Vec<_> = trace
                        .observations
                        .iter()
                        .filter_map(|o| {
                            let Action::Adapter { observation } = &o.action else {
                                return None;
                            };
                            (o.subject.effect == Some(record.id)).then_some(observation)
                        })
                        .collect();
                    assert_eq!(facts.len(), 5);
                    assert_eq!(facts[1].event, AdapterEvent::Response { status });
                    assert_eq!(
                        facts[2].event,
                        AdapterEvent::Usage {
                            usage: AdapterUsage {
                                input_tokens: Some(11 + index as u64 * 2),
                                total_tokens: Some(11 + index as u64 * 2),
                                ..AdapterUsage::default()
                            }
                        }
                    );
                    for fact in &facts {
                        assert_eq!(fact.attempt, Some((index + 1) as u64));
                        assert_eq!(
                            fact.host_attempt.map(std::num::NonZeroU64::get),
                            Some((index + 1) as u64)
                        );
                        if let Some(operation) = &operation {
                            assert_eq!(&fact.operation, operation);
                        } else {
                            operation = Some(fact.operation.clone());
                        }
                    }
                    assert_eq!(
                        facts[4].event,
                        AdapterEvent::Finished {
                            ending: AdapterEnding::Error {
                                boundary: rig::observe::AdapterErrorBoundary::ProviderResponse,
                                kind: "provider_response".into(),
                                status: Some(status),
                                retryable: true
                            }
                        }
                    );
                }
            },
        );
    }
}

#[test]
fn tool_result_starts_a_new_operation() {
    run(
        "observe_wire",
        "tool_result_starts_a_new_operation",
        Config {
            source: Source::of("observe_turns", "one_tool_unary"),
            ..Config::unary()
        },
        |cell| {
            cell.submit("Run this command: printf hi > out.txt");
            cell.drive();
            assert_eq!(cell.ending(), "settled");
            assert_eq!(
                std::fs::read_to_string(cell.dir.join("out.txt")).unwrap(),
                "hi"
            );
            assert_eq!(cell.count("rigcoder/provider_retry"), 0);
            rigcoder::observe::finalize(cell.app.world());
            let trace = cell.trace();
            let starts: Vec<_> = trace
                .observations
                .iter()
                .filter_map(|o| {
                    let Action::Adapter { observation } = &o.action else {
                        return None;
                    };
                    matches!(observation.event, AdapterEvent::Started { .. })
                        .then_some((o.subject.effect, observation))
                })
                .collect();
            assert_eq!(starts.len(), 2);
            assert_ne!(starts[0].0, starts[1].0);
            assert_ne!(starts[0].1.operation, starts[1].1.operation);
            assert_eq!(
                starts[0].1.host_attempt.map(std::num::NonZeroU64::get),
                Some(1)
            );
            assert_eq!(
                starts[1].1.host_attempt, None,
                "later calls use the bus's independent operation context"
            );
            assert!(starts.iter().all(|(_, fact)| fact.attempt == Some(1)));
        },
    );
}

#[test]
fn optional_response_id_stream() {
    let mut traces = Vec::new();
    for (name, with_ids) in [
        ("response_id_absent_stream", false),
        ("response_id_only_stream", true),
    ] {
        let source = derive(
            &cassette_path("observe_turns", "text_stream"),
            &cassette_path("observe_wire", name),
            |docs| {
                let content = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"pong\"}],\"role\":\"model\"},\"index\":0}]}\n\n";
                *body_of(&mut docs[0]) = if with_ids {
                    format!(
                        "data: {{\"responseId\":\"id_REDACTED_1\"}}\n\n{content}data: {{\"responseId\":\"id_REDACTED_2\"}}\n\n"
                    )
                } else {
                    content.to_owned()
                };
            },
        );
        run(
            "observe_wire",
            name,
            Config {
                source: Source::derived(source, "observe_turns", "text_stream"),
                ..Config::streamed()
            },
            |cell| {
                cell.submit("Reply with the single word: pong");
                cell.drive();
                assert_ne!(cell.ending(), "settled");
                assert_eq!(cell.log().records.len(), 1);
                rigcoder::observe::finalize(cell.app.world());
                let trace = cell.trace();
                assert!(trace.observations.iter().any(|fact| matches!(
                    &fact.action, Action::Adapter { observation }
                    if observation.event == AdapterEvent::TransportEof { after: 1, partial_bytes: 0 }
                )));
                traces.push(trace);
            },
        );
    }
    assert_eq!(
        super::comparison::compare(&traces[0], &traces[1]),
        super::comparison::Comparison::Equal
    );
    assert_eq!(
        normalized(
            "observations.json",
            &serde_json::to_string(&traces[0]).unwrap()
        ),
        normalized(
            "observations.json",
            &serde_json::to_string(&traces[1]).unwrap()
        ),
    );
}

#[test]
fn optional_response_id_unary() {
    let mut traces = Vec::new();
    for (name, id) in [
        ("response_id_absent_unary", None),
        ("response_id_only_unary", Some("id_REDACTED_1")),
    ] {
        let source = derive(
            &cassette_path("observe_turns", "text_unary"),
            &cassette_path("observe_wire", name),
            |docs| {
                let mut body = serde_json::json!({"candidates": []});
                if let Some(id) = id {
                    body["responseId"] = id.into();
                }
                *body_of(&mut docs[0]) = body.to_string();
            },
        );
        run(
            "observe_wire",
            name,
            Config {
                source: Source::derived(source, "observe_turns", "text_unary"),
                ..Config::unary()
            },
            |cell| {
                cell.submit("Reply with the single word: pong");
                cell.drive();
                assert_ne!(cell.ending(), "settled");
                assert_eq!(cell.count("rigcoder/provider_retry"), 0);
                rigcoder::observe::finalize(cell.app.world());
                let trace = cell.trace();
                let facts: Vec<_> = trace
                    .observations
                    .iter()
                    .filter_map(|o| {
                        let Action::Adapter { observation } = &o.action else {
                            return None;
                        };
                        Some(observation)
                    })
                    .collect();
                assert_eq!(facts.len(), 3);
                let closure = facts.last().unwrap();
                assert!(matches!(
                    closure.event,
                    AdapterEvent::Finished {
                        ending: AdapterEnding::Error { .. }
                    }
                ));
                assert_eq!(
                    closure
                        .analysis
                        .as_ref()
                        .and_then(|a| a.response_id.as_deref()),
                    id
                );
                traces.push(trace);
            },
        );
    }
    assert_eq!(
        super::comparison::compare(&traces[0], &traces[1]),
        super::comparison::Comparison::Equal
    );
    assert_eq!(
        normalized(
            "observations.json",
            &serde_json::to_string(&traces[0]).unwrap()
        ),
        normalized(
            "observations.json",
            &serde_json::to_string(&traces[1]).unwrap()
        )
    );
}

#[test]
fn unary_http_boundary() {
    run(
        "observe_wire",
        "unary_http_boundary",
        Config {
            source: Source::of("observe_turns", "text_unary"),
            witness: Some(WitnessConfig {
                clock: true,
                ..Default::default()
            }),
            ..Config::delivery(false)
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            assert_eq!(cell.ending(), "settled");
            assert!(cell.answer().to_lowercase().contains("pong"));
            let log = cell.log();
            assert_eq!(log.records.len(), 1);
            // This single dispatch has landed; finish capture as the CLI does.
            rigcoder::observe::finalize(cell.app.world());
            let trace = cell.trace();
            let observations: Vec<_> = trace
                .observations
                .iter()
                .filter_map(|o| {
                    let Action::Adapter { observation } = &o.action else {
                        return None;
                    };
                    Some((o, observation))
                })
                .collect();
            assert_eq!(observations.len(), 5);
            for (o, fact) in &observations {
                assert_eq!(o.subject.effect, Some(log.records[0].id));
                assert_eq!(o.subject.scope.as_deref(), Some("rigcoder/run/1"));
                assert_eq!(fact.operation, observations[0].1.operation);
                assert_eq!(fact.attempt, Some(1));
                assert_eq!(o.emitter.name, "rig-core/adapter");
            }
            assert_eq!(
                observations[0].1.event,
                AdapterEvent::Started {
                    method: "POST".into(),
                    route: "/models/{model}:generateContent".into()
                }
            );
            assert_eq!(
                observations[1].1.event,
                AdapterEvent::Response { status: 200 }
            );
            assert_eq!(
                observations[2].1.event,
                AdapterEvent::Usage {
                    usage: AdapterUsage {
                        input_tokens: Some(795),
                        output_tokens: Some(1),
                        total_tokens: Some(888),
                        reasoning_tokens: Some(92),
                        ..AdapterUsage::default()
                    }
                }
            );
            assert_eq!(
                observations[3].1.event,
                AdapterEvent::Provider {
                    verdict: AdapterVerdict {
                        finish_reason: Some("STOP".into()),
                        model: Some("gemini-3.8-flash".into()),
                        ..AdapterVerdict::default()
                    }
                }
            );
            assert_eq!(
                observations[3]
                    .1
                    .analysis
                    .as_ref()
                    .unwrap()
                    .response_id
                    .as_deref(),
                Some("id_REDACTED_1")
            );
            assert_eq!(
                observations[4].1.event,
                AdapterEvent::Finished {
                    ending: AdapterEnding::Decoded
                }
            );
        },
    );
}

#[test]
fn stream_http_boundary() {
    stream_boundary(
        "stream_http_boundary",
        Source::of("observe_turns", "text_stream"),
        0,
        true,
    );
}

#[test]
fn partial_frame_boundary() {
    stream_boundary(
        "partial_frame_boundary",
        Source::derived(
            cassette_path("observe_failures", "transport_cut_stream"),
            "observe_turns",
            "text_stream",
        ),
        204,
        false,
    );
}

#[test]
fn terminal_then_partial_frame() {
    let source = derive(
        &cassette_path("observe_turns", "text_stream"),
        &cassette_path("observe_wire", "terminal_then_partial_frame"),
        |docs| body_of(&mut docs[0]).push_str("data: {"),
    );
    stream_boundary(
        "terminal_then_partial_frame",
        Source::derived(source, "observe_turns", "text_stream"),
        7,
        true,
    );
}

fn stream_boundary(name: &str, source: Source, partial_bytes: usize, terminal: bool) {
    run(
        "observe_wire",
        name,
        Config {
            source,
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            if !terminal {
                assert_ne!(cell.ending(), "settled");
            } else {
                assert_eq!(cell.ending(), "settled");
                assert!(cell.answer().to_lowercase().contains("pong"));
            }
            let log = cell.log();
            assert_eq!(log.records.len(), 1);
            rigcoder::observe::finalize(cell.app.world());
            let trace = cell.trace();
            let facts: Vec<_> = trace
                .observations
                .iter()
                .filter_map(|o| {
                    let Action::Adapter { observation } = &o.action else {
                        return None;
                    };
                    assert_eq!(o.subject.effect, Some(log.records[0].id));
                    assert_eq!(o.subject.scope.as_deref(), Some("rigcoder/run/1"));
                    assert_eq!(observation.attempt, Some(1));
                    Some(observation)
                })
                .collect();
            assert_eq!(facts.len(), if terminal { 8 } else { 6 });
            assert!(facts.iter().all(|f| f.operation == facts[0].operation));
            assert_eq!(
                facts[0].event,
                AdapterEvent::Started {
                    method: "POST".into(),
                    route: "/models/{model}:streamGenerateContent".into(),
                }
            );
            assert_eq!(facts[1].event, AdapterEvent::Response { status: 200 });
            for (index, pair) in facts[2..facts.len() - 2].chunks_exact(2).enumerate() {
                assert_eq!(
                    pair[0].event,
                    AdapterEvent::Usage {
                        usage: AdapterUsage {
                            input_tokens: Some(795),
                            output_tokens: Some(1),
                            total_tokens: Some(856),
                            reasoning_tokens: Some(60),
                            ..AdapterUsage::default()
                        }
                    }
                );
                assert_eq!(
                    pair[1].event,
                    AdapterEvent::Provider {
                        verdict: AdapterVerdict {
                            finish_reason: (index == 1).then(|| "STOP".into()),
                            model: Some("gemini-3.8-flash".into()),
                            ..AdapterVerdict::default()
                        }
                    }
                );
                assert_eq!(
                    pair[1].analysis.as_ref().unwrap().response_id.as_deref(),
                    Some("id_REDACTED_1")
                );
            }
            assert_eq!(
                facts[facts.len() - 2].event,
                AdapterEvent::TransportEof {
                    after: if terminal { 2 } else { 1 },
                    partial_bytes,
                }
            );
            if !terminal {
                assert_eq!(
                    facts.last().unwrap().event,
                    AdapterEvent::Finished {
                        ending: AdapterEnding::PartialFrame {
                            byte_count: partial_bytes,
                            after: 1
                        }
                    }
                );
            } else {
                assert_eq!(
                    facts.last().unwrap().event,
                    AdapterEvent::Finished {
                        ending: AdapterEnding::Terminal
                    }
                );
            }
        },
    );
}

#[test]
fn blocked_prompt_boundary() {
    let source = derive(
        &cassette_path("observe_turns", "text_unary"),
        &cassette_path("observe_wire", "blocked_prompt_boundary"),
        |docs| {
            *body_of(&mut docs[0]) = serde_json::json!({
                "promptFeedback": {"blockReason": "SAFETY"},
                "usageMetadata": {"promptTokenCount": 795, "totalTokenCount": 795}
            })
            .to_string();
        },
    );
    run(
        "observe_wire",
        "blocked_prompt_boundary",
        Config {
            source: Source::derived(source, "observe_turns", "text_unary"),
            ..Config::unary()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            assert_ne!(cell.ending(), "settled");
            assert_eq!(cell.count("rigcoder/provider_retry"), 0);
            rigcoder::observe::finalize(cell.app.world());
            let trace = cell.trace();
            let facts: Vec<_> = trace
                .observations
                .iter()
                .filter_map(|o| {
                    let Action::Adapter { observation } = &o.action else {
                        return None;
                    };
                    Some(observation)
                })
                .collect();
            assert_eq!(facts.len(), 5);
            assert_eq!(
                facts[3].event,
                AdapterEvent::Provider {
                    verdict: AdapterVerdict {
                        block_reason: Some("SAFETY".into()),
                        ..AdapterVerdict::default()
                    }
                }
            );
            assert!(matches!(
                facts[4].event,
                AdapterEvent::Finished {
                    ending: AdapterEnding::Error {
                        retryable: false,
                        ..
                    }
                }
            ));
        },
    );
}

#[test]
fn retry_headers_unary() {
    let source = derive(
        &cassette_path("observe_failures", "rate_limited_unary"),
        &cassette_path("observe_wire", "retry_headers_unary"),
        |docs| {
            let headers = docs[0]["then"]["header"].as_sequence_mut().unwrap();
            for (name, value) in [("retry-after", "0"), ("x-request-id", "req_REDACTED_1")] {
                headers.push(
                    serde_yaml::to_value(serde_json::json!({"name": name, "value": value}))
                        .unwrap(),
                );
            }
        },
    );
    run(
        "observe_wire",
        "retry_headers_unary",
        Config {
            retries: 1,
            witness: Some(WitnessConfig {
                clock: true,
                ..Default::default()
            }),
            source: Source::derived(source, "observe_turns", "text_unary"),
            ..Config::unary()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            assert_eq!(cell.ending(), "settled");
            assert_eq!(cell.count("rigcoder/provider_retry"), 1);
            let log = cell.log();
            assert_eq!(log.records.len(), 2);
            rigcoder::observe::finalize(cell.app.world());
            let trace = cell.trace();
            let failed: Vec<_> = trace
                .observations
                .iter()
                .filter_map(|o| {
                    let Action::Adapter { observation } = &o.action else {
                        return None;
                    };
                    (o.subject.effect == Some(log.records[0].id)).then_some(observation)
                })
                .collect();
            assert_eq!(failed.len(), 4);
            let successful: Vec<_> = trace
                .observations
                .iter()
                .filter_map(|o| {
                    let Action::Adapter { observation } = &o.action else {
                        return None;
                    };
                    (o.subject.effect == Some(log.records[1].id)).then_some(observation)
                })
                .collect();
            assert_eq!(successful.len(), 5);
            let handlers: Vec<_> = trace
                .observations
                .iter()
                .filter(|o| matches!(o.action, Action::Landed { .. }))
                .collect();
            assert_eq!(handlers.len(), 2);
            for (handler, record) in handlers.iter().zip(&log.records) {
                assert_eq!(handler.subject.effect, Some(record.id));
                assert!(
                    matches!(&handler.action, Action::Landed { outcome } if *outcome == rig::observe::OutcomeSummary::of(&record.outcome))
                );
            }
            assert!(failed.iter().all(|f| f.attempt == Some(1)
                && f.host_attempt.map(std::num::NonZeroU64::get) == Some(1)));
            assert!(successful.iter().all(|f| f.operation == failed[0].operation
                && f.attempt == Some(2)
                && f.host_attempt.map(std::num::NonZeroU64::get) == Some(2)));
            assert_eq!(failed[1].event, AdapterEvent::Response { status: 429 });
            let headers = failed[1]
                .analysis
                .as_ref()
                .unwrap()
                .headers
                .as_ref()
                .unwrap();
            assert_eq!(headers.get("retry-after").map(String::as_str), Some("0"));
            assert_eq!(
                headers.get("x-request-id").map(String::as_str),
                Some("req_REDACTED_1")
            );
            assert!(!headers.contains_key("date"));
            assert!(
                matches!(&failed[2].event, AdapterEvent::ErrorEnvelope { error }
            if error.code.as_deref() == Some("429") && error.status.as_deref() == Some("RESOURCE_EXHAUSTED"))
            );
            assert_eq!(
                failed[3].event,
                AdapterEvent::Finished {
                    ending: AdapterEnding::Error {
                        kind: "provider_response".into(),
                        boundary: rig::observe::AdapterErrorBoundary::ProviderResponse,
                        status: Some(429),
                        retryable: true,
                    }
                }
            );
        },
    );
}
