use super::*;

#[test]
fn digest_consumes_direct_provider_attempts_without_bus_records() {
    for (input, status, provenance) in [
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_wire/direct_unary/observations.json"
            ),
            200,
            rigcoder::observe::RecordingProvenance::Live,
        ),
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_wire/direct_stream/observations.json"
            ),
            200,
            rigcoder::observe::RecordingProvenance::Live,
        ),
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_wire/direct_failure/observations.json"
            ),
            401,
            rigcoder::observe::RecordingProvenance::Derived,
        ),
    ] {
        let digest = observed(input);
        assert!(digest.complete);
        assert_eq!(digest.recording_provenance, Some(provenance));
        assert_eq!(digest.attempts.len(), 1);
        let attempt = &digest.attempts[0];
        assert_eq!(attempt.operation, "direct/1");
        assert_eq!(attempt.status, Some(status));
        assert_eq!(attempt.subject.scope.as_deref(), Some("direct"));
        assert!(attempt.subject.effect.is_none());
        assert!(attempt.ending.is_some());
        assert!(digest.endings.is_empty(), "no invented agent run outcome");
        assert!(
            digest.last_failure.is_none(),
            "no invented host failure decision"
        );
    }
}

#[test]
fn digest_preserves_named_holds_without_inventing_cleanup_releases() {
    let cases = [
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_gates/ask_approve_unary/observations.json"
            ),
            1,
            1,
        ),
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_gates/ask_refuse_unary/observations.json"
            ),
            1,
            0,
        ),
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_interruptions/cancel_at_hold_stream/observations.json"
            ),
            1,
            0,
        ),
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_interruptions/despawn_at_hold_stream/observations.json"
            ),
            1,
            0,
        ),
    ];
    for (input, acquired, released) in cases {
        let digest = observed(input);
        assert!(digest.trace_present);
        assert_eq!(digest.dropped, Some(0));
        assert_eq!(
            digest.finalized,
            Some(false),
            "matrix snapshots are not finalized"
        );
        assert!(!digest.complete);
        assert_eq!(digest.holds, 1, "approval decision count stays separate");
        assert_eq!(
            digest
                .hold_transitions
                .iter()
                .filter(|hold| hold.acquired)
                .count(),
            acquired
        );
        assert_eq!(
            digest
                .hold_transitions
                .iter()
                .filter(|hold| !hold.acquired)
                .count(),
            released
        );
        let subject = &digest.hold_transitions[0].subject;
        assert!(subject.order.is_some());
        assert!(subject.scope.is_some());
        for hold in &digest.hold_transitions {
            assert_eq!(hold.owner.name, "rigcoder/approval");
            assert_eq!(&hold.subject, subject);
        }
        // A truncated trace retains its transitions but cannot establish that
        // the last observed owner still holds a call or that cleanup occurred.
        let mut artifact: rigcoder::observe::ObservationArtifact =
            serde_json::from_str(input).unwrap();
        artifact.trace.finalized = false;
        artifact.trace.dropped = 1;
        let incomplete = observed(&serde_json::to_string(&artifact).unwrap());
        assert!(!incomplete.complete);
        assert_eq!(incomplete.dropped, Some(1));
        assert_eq!(incomplete.hold_transitions, digest.hold_transitions);
    }
}

#[test]
fn digest_joins_batch_and_approval_owners_to_each_tool() {
    for input in [
        include_str!(
            "../../../../fixtures/evidence/gemini/observe_turns/calls4_c1_unary/observations.json"
        ),
        include_str!(
            "../../../../fixtures/evidence/gemini/observe_turns/calls4_c1_stream/observations.json"
        ),
    ] {
        let digest = observed(input);
        assert!(digest.trace_present);
        assert_eq!(digest.dropped, Some(0));
        assert_eq!(digest.finalized, Some(false));
        assert!(!digest.complete);
        let mut active = std::collections::BTreeSet::new();
        let mut acquisitions = std::collections::BTreeMap::new();
        for hold in &digest.hold_transitions {
            assert!(hold.subject.scope.is_some());
            let key = (
                hold.subject.scope.as_deref(),
                hold.subject.order.unwrap(),
                hold.owner.name.as_str(),
            );
            if hold.acquired {
                assert!(active.insert(key));
                *acquisitions.entry(hold.owner.name.as_str()).or_insert(0) += 1;
            } else {
                assert!(active.remove(&key));
            }
        }
        assert!(active.is_empty());
        assert_eq!(
            acquisitions,
            std::collections::BTreeMap::from([("rig-ecs/batch", 3), ("rigcoder/approval", 4),])
        );
        assert_eq!(digest.holds, 0, "auto approval uses prepared decisions");
    }
}

#[test]
fn digest_retains_recording_origin_independently_of_replay_execution() {
    use rigcoder::observe::{ExecutionMode, RecordingProvenance};
    let cases = [
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_lineage/clock/observations.json"
            ),
            ExecutionMode::CassetteReplay,
            RecordingProvenance::Live,
        ),
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_wire/retry_headers_unary/observations.json"
            ),
            ExecutionMode::CassetteReplay,
            RecordingProvenance::Derived,
        ),
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_interruptions/cancel_mid_stream/observations.json"
            ),
            ExecutionMode::PacedReplay,
            RecordingProvenance::Live,
        ),
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_interruptions/cancel_before_dispatch/observations.json"
            ),
            ExecutionMode::LocalOnly,
            RecordingProvenance::NotApplicable,
        ),
    ];
    for (input, mode, provenance) in cases {
        let digest = observed(input);
        assert_eq!(digest.measurement_context.unwrap().execution_mode, mode);
        assert_eq!(digest.recording_provenance, Some(provenance));
        // Missing historical provenance cannot be recovered from replay mode.
        let mut legacy: serde_json::Value = serde_json::from_str(input).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("recording_provenance");
        let legacy = observed(&serde_json::to_string(&legacy).unwrap());
        assert_eq!(legacy.measurement_context.unwrap().execution_mode, mode);
        assert_eq!(legacy.recording_provenance, None);
    }
}

#[test]
fn digest_preserves_execution_and_clock_labels_without_changing_task_facts() {
    use rigcoder::observe::{ClockSource, ExecutionMode, MeasurementContext, ObservationArtifact};
    // Synthetic metadata variants over the same replay bytes, never live
    // performance evidence. No new trial or evaluator is run here.
    let source = include_str!(
        "../../../../fixtures/evidence/gemini/observe_lineage/clock/observations.json"
    );
    let mut artifact: ObservationArtifact = serde_json::from_str(source).unwrap();
    let mut baseline = None;
    for mode in [
        ExecutionMode::Live,
        ExecutionMode::CassetteReplay,
        ExecutionMode::PacedReplay,
        ExecutionMode::LocalOnly,
        ExecutionMode::EffectLogReplay,
    ] {
        let context = MeasurementContext {
            execution_mode: mode,
            clock_source: ClockSource::Scripted,
        };
        artifact.measurement_context = Some(context.clone());
        let digest = observed(&serde_json::to_string(&artifact).unwrap());
        assert_eq!(digest.measurement_context, Some(context));
        let mut facts_only = digest.clone();
        facts_only.measurement_context = None;
        if let Some(baseline) = &baseline {
            assert_eq!(baseline, &facts_only);
        } else {
            baseline = Some(facts_only);
        }
    }
    artifact.measurement_context = None;
    assert!(
        observed(&serde_json::to_string(&artifact).unwrap())
            .measurement_context
            .is_none()
    );
}

#[test]
fn digest_retains_recording_context_and_provider_endings() {
    let timed = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_lineage/clock/observations.json"
    ));
    assert_eq!(timed.attempts.len(), 1);
    assert_eq!(
        timed.measurement_context,
        Some(rigcoder::observe::MeasurementContext {
            execution_mode: rigcoder::observe::ExecutionMode::CassetteReplay,
            clock_source: rigcoder::observe::ClockSource::Scripted,
        })
    );
    assert_eq!(timed.endings.len(), 1);
    assert_eq!(
        timed.endings[0].subject.scope.as_deref(),
        Some("rigcoder/run/1")
    );
    assert_eq!(timed.endings[0].ending, "settled");
    let untimed = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_lineage/clockless/observations.json"
    ));
    assert_eq!(untimed.attempts[0].ending, timed.attempts[0].ending);
    assert_eq!(
        untimed.measurement_context.as_ref().unwrap().clock_source,
        rigcoder::observe::ClockSource::Absent
    );
    let cancelled = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_interruptions/cancel_before_dispatch/observations.json"
    ));
    assert_eq!(cancelled.endings.len(), 1);
    assert_eq!(
        cancelled
            .measurement_context
            .as_ref()
            .unwrap()
            .execution_mode,
        rigcoder::observe::ExecutionMode::LocalOnly
    );
    assert_eq!(cancelled.endings[0].ending, "cancelled");
    assert!(cancelled.attempts.is_empty());
    let unary = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/unary_http_boundary/observations.json"
    ));
    assert_eq!(unary.attempts.len(), 1);
}

#[test]
fn digest_retains_provider_outcomes_across_retry() {
    let digest = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/retry_headers_unary/observations.json"
    ));
    assert_eq!(digest.attempts.len(), 2);
    assert!(matches!(
        digest.attempts[0].ending,
        Some(rig::observe::AdapterEnding::Error { .. })
    ));
    assert_eq!(
        digest.attempts[1].ending,
        Some(rig::observe::AdapterEnding::Decoded)
    );
}

#[test]
fn digest_keeps_host_rejection_separate_from_provider_success() {
    let replaced = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_gates/replaced_unary/observations.json"
    ));
    let failure = replaced.last_failure.as_ref().unwrap();
    assert_eq!(failure.boundary, rigcoder::failure::FailureBoundary::Host);
    assert!(failure.adapter.is_none());
    assert_eq!(replaced.attempts.len(), 1);
    assert_eq!(replaced.attempts[0].status, Some(200));
    assert_eq!(replaced.provider_retries, 0);
    let denied = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_gates/discarded/observations.json"
    ));
    assert!(denied.attempts.is_empty());
    assert_eq!(
        denied.last_failure.as_ref().unwrap().boundary,
        rigcoder::failure::FailureBoundary::Host
    );
    let cancelled = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_interruptions/cancel_mid_stream/observations.json"
    ));
    assert_eq!(cancelled.last_failure.as_ref().unwrap().kind, "cancelled");
    assert!(cancelled.last_failure.as_ref().unwrap().adapter.is_none());
    assert_eq!(cancelled.attempts.len(), 1);
    assert_eq!(cancelled.attempts[0].status, Some(200));
}

#[test]
fn digest_retains_content_type_failure_boundary_and_actual_http_status() {
    let digest = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/invalid_stream_content_type/observations.json"
    ));
    assert!(digest.complete);
    assert_eq!(digest.attempts.len(), 1);
    assert_eq!(digest.attempts[0].status, Some(200));
    let failure = digest.last_failure.as_ref().unwrap();
    assert_eq!(failure.kind, "provider");
    assert_eq!(failure.http_status, None);
    assert_eq!(failure.boundary, rigcoder::failure::FailureBoundary::Decode);
    assert_eq!(
        failure.adapter.as_ref().unwrap().ending,
        digest.attempts[0].ending
    );
    assert_eq!(digest.provider_retries, 0);
}

#[test]
fn digest_consumes_a_rejected_submission_without_provider_facts() {
    let digest = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_host/invalid_submission/observations.json"
    ));
    assert!(digest.complete);
    assert!(digest.attempts.is_empty());
    assert!(digest.endings.is_empty(), "submission did not create a run");
    let failure = digest.last_failure.as_ref().unwrap();
    assert_eq!(failure.kind, "invalid_configuration");
    assert!(failure.adapter.is_none());
    assert!(failure.http_status.is_none());
    let trial = facts(include_str!(
        "../../../../fixtures/evidence/gemini/observe_host/invalid_submission/transcript.jsonl"
    ));
    assert_eq!(trial.failure.as_ref(), Some(failure));
    assert_eq!(trial.ending, "invalid_configuration");
    assert!(trial.no_settle);
}

#[test]
fn structured_failures_support_trial_investigation_without_text_classification() {
    use rig::observe::{Action, ObservationTrace};
    use rigcoder::failure::FailureDetail;
    let mut trace: ObservationTrace = serde_json::from_str(include_str!(
        "../../../../fixtures/evidence/gemini/observe_failures/rate_limited_unary/observations.json"
    ))
    .unwrap();
    let mut failure = FailureDetail::report(
        "provider",
        &rig::error::ErrorReport::new(rig::error::ErrorKind::Http { status: Some(429) }, "quota")
            .with_retryable(true),
        &[],
    );
    failure.attach("rigcoder/run/1", &trace);
    // Synthetic host projection of the recorded failure, using the production
    // serializer and digest. No provider request or task score is fabricated.
    let mut host = trace
        .observations
        .iter()
        .find(|o| {
            matches!(&o.action,
                Action::Host { kind, .. } if kind == "rigcoder/provider_retry"
            )
        })
        .unwrap()
        .clone();
    host.action = Action::Host {
        kind: "rigcoder/failure".into(),
        payload: serde_json::to_value(&failure).unwrap(),
    };
    trace.observations.push(host);
    let digest = observed(&serde_json::to_string(&trace).unwrap());
    assert_eq!(digest.last_failure.as_ref(), Some(&failure));
    let failed_attempt = digest
        .last_failure
        .as_ref()
        .unwrap()
        .adapter
        .as_ref()
        .unwrap();
    let operation: Vec<_> = digest
        .attempts
        .iter()
        .filter(|a| a.operation == failed_attempt.operation)
        .collect();
    assert_eq!(operation.len(), 2);
    assert_eq!(operation[0].status, Some(429));
    assert_eq!(operation[1].status, Some(200));
    assert_eq!(
        digest.retry_decisions[0].operation.as_deref(),
        Some(failed_attempt.operation.as_str())
    );
    assert_eq!(
        digest.endings.last().map(|run| run.ending.as_str()),
        Some("settled")
    );

    let transcript = serde_json::json!({"kind":"failed", "reason":failure}).to_string();
    let trial = facts(&transcript);
    assert_eq!(trial.ending, "http");
    assert_eq!(trial.failure, Some(failure));
    assert!(trial.no_settle);
    let unstructured = facts(r#"{"kind":"failed","reason":"provider 503 replay"}"#);
    assert_eq!(unstructured.ending, "unknown");
    assert!(unstructured.failure.is_none());
    assert!(unstructured.no_settle);
}

#[test]
fn exhausted_retries_keep_one_operation_and_separate_failed_usage() {
    for packet in [
        include_str!(
            "../../../../fixtures/evidence/gemini/observe_wire/retry_exhaustion_unary/observations.json"
        ),
        include_str!(
            "../../../../fixtures/evidence/gemini/observe_wire/retry_exhaustion_stream/observations.json"
        ),
    ] {
        let digest = observed(packet);
        assert!(digest.complete);
        assert_eq!(digest.attempts.len(), 2);
        assert_eq!(digest.provider_retries, 1);
        assert_eq!(digest.retry_decisions.len(), 1);
        assert_eq!(
            digest
                .endings
                .iter()
                .map(|run| run.ending.as_str())
                .collect::<Vec<_>>(),
            ["provider", "provider"]
        );
        let first = &digest.attempts[0];
        let second = &digest.attempts[1];
        assert_eq!(first.operation, second.operation);
        assert_ne!(first.subject.scope, second.subject.scope);
        assert_ne!(first.subject.effect, second.subject.effect);
        assert_eq!((first.attempt, second.attempt), (1, 2));
        assert_eq!(first.host_attempt.map(std::num::NonZeroU64::get), Some(1));
        assert_eq!(second.host_attempt.map(std::num::NonZeroU64::get), Some(2));
        assert_eq!(first.usage.as_ref().unwrap().total_tokens, Some(11));
        assert_eq!(second.usage.as_ref().unwrap().total_tokens, Some(13));
        assert!(digest.attempts.iter().all(|a| matches!(
            a.ending,
            Some(rig::observe::AdapterEnding::Error {
                retryable: true,
                ..
            })
        )));
        assert_eq!(
            digest.retry_decisions[0].operation.as_deref(),
            Some(first.operation.as_str())
        );
        assert_eq!(digest.retry_decisions[0].retry, Some(1));
    }
}

#[test]
fn terminal_after_corruption_does_not_turn_product_failure_into_success() {
    let digest = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_failures/malformed_frame_stream/observations.json"
    ));
    assert_eq!(
        digest
            .endings
            .iter()
            .map(|run| run.ending.as_str())
            .collect::<Vec<_>>(),
        ["provider"]
    );
    assert_eq!(digest.provider_retries, 0);
    assert_eq!(digest.attempts.len(), 1);
    assert_eq!(
        digest.attempts[0].verdict.finish_reason.as_deref(),
        Some("STOP")
    );
    assert_eq!(
        digest.attempts[0].ending,
        Some(rig::observe::AdapterEnding::Terminal)
    );
    assert!(
        digest
            .adapter
            .iter()
            .any(|a| a.fact.event == rig::observe::AdapterEvent::Corrupt { frame: 1 })
    );
}

#[test]
fn response_id_only_metadata_reaches_digest_without_inventing_a_verdict() {
    let absent = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/response_id_absent_unary/observations.json"
    ));
    let identified = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/response_id_only_unary/observations.json"
    ));
    assert!(absent.complete && identified.complete);
    assert_eq!(absent.attempts.len(), 1);
    assert_eq!(identified.attempts.len(), 1);
    let left = &absent.attempts[0];
    let right = &identified.attempts[0];
    assert_eq!(left.analysis.response_id, None);
    assert_eq!(right.analysis.response_id.as_deref(), Some("id_REDACTED_1"));
    assert_eq!(right.verdict, rig::observe::AdapterVerdict::default());
    assert_eq!(left.ending, right.ending);
    assert!(matches!(
        right.ending,
        Some(rig::observe::AdapterEnding::Error { .. })
    ));
    assert_eq!(absent.endings, identified.endings);
}

#[test]
fn missing_unfinalized_and_dropped_evidence_are_not_complete() {
    let invalid = observed("not JSON");
    assert!(!invalid.complete);
    assert!(!invalid.trace_present);
    assert_eq!(invalid.dropped, None);
    assert_eq!(invalid.finalized, None);

    for (finalized, dropped, complete) in [(false, 0, false), (true, 2, false), (true, 0, true)] {
        let trace = serde_json::json!({
            "observations": [], "finalized": finalized, "dropped": dropped
        });
        let facts = observed(&trace.to_string());
        assert!(facts.trace_present);
        assert_eq!(facts.complete, complete);
        assert_eq!(facts.finalized, Some(finalized));
        assert_eq!(facts.dropped, Some(dropped));
    }
}

#[test]
fn diagnostic_selection_keeps_the_original_trial_and_scoring_population() {
    let root = std::env::temp_dir().join(format!(
        "rigcoder-diagnostic-selection-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    for (name, reward, observations, transcript) in [
        (
            "retried__1",
            "0",
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_failures/rate_limited_unary/observations.json"
            ),
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_failures/rate_limited_unary/transcript.jsonl"
            ),
        ),
        (
            "clean__1",
            "1",
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_wire/unary_http_boundary/observations.json"
            ),
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_wire/unary_http_boundary/transcript.jsonl"
            ),
        ),
    ] {
        let trial = root.join(name);
        std::fs::create_dir_all(trial.join("agent")).unwrap();
        std::fs::create_dir_all(trial.join("verifier")).unwrap();
        std::fs::write(trial.join("agent/observations.json"), observations).unwrap();
        std::fs::write(trial.join("agent/transcript.jsonl"), transcript).unwrap();
        std::fs::write(trial.join("verifier/reward.txt"), reward).unwrap();
    }
    let digest = job(&root, &root).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    let selected: Vec<_> = digest
        .trials
        .iter()
        .filter(|trial| {
            trial
                .observed
                .last_failure
                .as_ref()
                .is_some_and(|failure| failure.kind == "http")
        })
        .collect();
    assert_eq!(selected.len(), 1);
    let trial = selected[0];
    assert_eq!(trial.trial, "retried__1");
    assert_eq!(trial.reward, Some(0.0));
    assert_eq!(trial.ending, "settled");
    let operation = &trial
        .observed
        .last_failure
        .as_ref()
        .unwrap()
        .adapter
        .as_ref()
        .unwrap()
        .operation;
    let attempts: Vec<_> = trial
        .observed
        .attempts
        .iter()
        .filter(|attempt| &attempt.operation == operation)
        .collect();
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        (attempts[0].status, attempts[1].status),
        (Some(429), Some(200))
    );
    assert_eq!(
        digest.trials.len(),
        2,
        "attempts are not independent task trials"
    );
    assert_eq!((digest.failed.trials, digest.passed.trials), (1, 1));
}

#[test]
fn job_keeps_failed_task_score_when_provider_run_settled_with_complete_evidence() {
    let root = std::env::temp_dir().join(format!(
        "rigcoder-digest-evidence-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let trial = root.join("task__trial");
    std::fs::create_dir_all(trial.join("agent")).unwrap();
    std::fs::create_dir_all(trial.join("verifier")).unwrap();
    std::fs::write(
        trial.join("agent/transcript.jsonl"),
        "{\"kind\":\"settled\"}\n",
    )
    .unwrap();
    std::fs::write(trial.join("verifier/reward.txt"), "0").unwrap();
    std::fs::write(trial.join("agent/observations.json"), include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/unary_http_boundary/observations.json"
    )).unwrap();
    let digest = job(&root, &root).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(digest.trials.len(), 1);
    assert_eq!(digest.trials[0].trial, "task__trial");
    assert_eq!(digest.trials[0].task.as_deref(), Some("task"));
    assert_eq!(
        digest.trials[0].task_source,
        Some(TaskSource::DirectoryName)
    );
    assert_eq!(digest.trials[0].attempt, None);
    assert!(digest.evaluation.is_none());
    assert!(digest.trials[0].observed.complete);
    assert_eq!(digest.trials[0].observed.attempts.len(), 1);
    assert_eq!(digest.trials[0].observed.attempts[0].status, Some(200));
    assert_eq!(
        digest.trials[0].observed.attempts[0].ending,
        Some(rig::observe::AdapterEnding::Decoded)
    );
    assert_eq!(digest.trials[0].ending, "settled");
    assert!(!digest.trials[0].no_settle);
    assert_eq!(digest.trials[0].reward, Some(0.0));
    assert_eq!(digest.failed.trials, 1);
    assert_eq!(digest.passed.trials, 0);
}

#[test]
fn digest_consumes_the_product_unary_adapter_packet_with_its_effect_link() {
    let trace = include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/unary_http_boundary/observations.json"
    );
    let digest = observed(trace);
    assert!(digest.complete);
    assert_eq!(digest.adapter.len(), 5);
    let first = &digest.adapter[0];
    assert!(first.subject.effect.is_some());
    assert_eq!(first.subject.scope.as_deref(), Some("rigcoder/run/1"));
    assert_eq!(first.fact.attempt, Some(1));
    assert_eq!(
        digest.adapter[1].fact.event,
        rig::observe::AdapterEvent::Response { status: 200 }
    );
    assert!(
        digest.adapter.iter().all(
            |fact| fact.subject == first.subject && fact.fact.operation == first.fact.operation
        )
    );
    let json = serde_json::to_value(digest).unwrap();
    assert_eq!(json["adapter"][1]["fact"]["event"]["status"], 200);
    assert_eq!(json["attempts"][0]["usage"]["total_tokens"], 888);
    assert_eq!(json["attempts"][0]["verdict"]["finish_reason"], "STOP");
    assert_eq!(json["attempts"][0]["verdict"]["model"], "gemini-3.8-flash");
    assert_eq!(
        json["attempts"][0]["analysis"]["response_id"],
        "id_REDACTED_1"
    );
}

#[test]
fn digest_distinguishes_stream_terminal_from_partial_frame_in_product_packets() {
    use rig::observe::{AdapterEnding, AdapterEvent};
    for (packet, ending) in [
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_wire/stream_http_boundary/observations.json"
            ),
            AdapterEnding::Terminal,
        ),
        (
            include_str!(
                "../../../../fixtures/evidence/gemini/observe_wire/partial_frame_boundary/observations.json"
            ),
            AdapterEnding::PartialFrame {
                byte_count: 204,
                after: 1,
            },
        ),
    ] {
        let digest = observed(packet);
        assert!(digest.complete);
        assert_eq!(digest.attempts.len(), 1);
        assert_eq!(
            digest.adapter.last().unwrap().fact.event,
            AdapterEvent::Finished { ending }
        );
        assert_eq!(
            digest.attempts[0].usage.as_ref().unwrap().total_tokens,
            Some(856)
        );
        assert!(
            digest
                .adapter
                .iter()
                .all(|f| f.fact.operation == digest.adapter[0].fact.operation
                    && f.subject == digest.adapter[0].subject
                    && f.fact.attempt == Some(1))
        );
    }
}

#[test]
fn rejected_product_response_keeps_usage_without_claiming_success() {
    let packet = include_str!(
        "../../../../fixtures/evidence/gemini/observe_turns/empty_candidate_unary/observations.json"
    );
    let digest = observed(packet);
    assert_eq!(digest.attempts.len(), 1);
    let attempt = &digest.attempts[0];
    assert_eq!(attempt.status, Some(200));
    assert_eq!(attempt.usage.as_ref().unwrap().total_tokens, Some(804));
    assert_eq!(attempt.verdict.finish_reason.as_deref(), Some("MAX_TOKENS"));
    assert!(
        matches!(&attempt.ending, Some(rig::observe::AdapterEnding::Error { kind, retryable: false, .. }) if kind == "response")
    );
    assert_eq!(
        digest
            .endings
            .iter()
            .map(|run| run.ending.as_str())
            .collect::<Vec<_>>(),
        ["provider"]
    );
    assert!(
        !digest.complete,
        "this existing cell records an unfinalized snapshot"
    );
}

#[test]
fn digest_consumes_block_and_retry_diagnostics_from_product_packets() {
    let blocked = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/blocked_prompt_boundary/observations.json"
    ));
    assert!(blocked.complete);
    assert_eq!(blocked.attempts.len(), 1);
    assert_eq!(
        blocked.attempts[0].verdict.block_reason.as_deref(),
        Some("SAFETY")
    );
    assert_eq!(
        blocked.attempts[0].usage.as_ref().unwrap().total_tokens,
        Some(795)
    );
    let retried = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/retry_headers_unary/observations.json"
    ));
    assert!(retried.complete);
    assert_eq!(retried.provider_retries, 1);
    assert_eq!(retried.attempts.len(), 2);
    assert_eq!(retried.attempts[0].status, Some(429));
    assert_eq!(
        retried.attempts[0]
            .error_envelope
            .as_ref()
            .unwrap()
            .code
            .as_deref(),
        Some("429")
    );
    let headers = retried.attempts[0].analysis.headers.as_ref().unwrap();
    assert_eq!(headers.get("retry-after").map(String::as_str), Some("0"));
    assert_eq!(retried.attempts[1].status, Some(200));
    assert_eq!(
        retried.attempts[1].verdict.finish_reason.as_deref(),
        Some("STOP")
    );
    assert_eq!(retried.attempts[0].usage, None);
}

#[test]
fn digest_keeps_partial_transport_eof_even_when_the_product_settled() {
    let digest = observed(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/terminal_then_partial_frame/observations.json"
    ));
    assert!(
        digest.complete,
        "capture completeness is independent of body completeness"
    );
    assert_eq!(
        digest
            .endings
            .iter()
            .map(|run| run.ending.as_str())
            .collect::<Vec<_>>(),
        ["settled"]
    );
    assert_eq!(digest.attempts.len(), 1);
    let attempt = &digest.attempts[0];
    assert_eq!(attempt.ending, Some(rig::observe::AdapterEnding::Terminal));
    assert_eq!(
        attempt.transport_eof,
        Some(ObservedEof {
            after: 2,
            partial_bytes: 7
        })
    );
    assert_eq!(attempt.usage.as_ref().unwrap().total_tokens, Some(856));
}

#[test]
fn cumulative_usage_is_replaced_and_attempts_remain_separate() {
    use rig::observe::{Action, AdapterEnding, AdapterEvent, AdapterUsage, ObservationTrace};
    // Offline synthetic retry history built from a real finalized trace.
    let mut trace: ObservationTrace = serde_json::from_str(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/stream_http_boundary/observations.json"
    ))
    .unwrap();
    trace
        .observations
        .retain(|o| matches!(o.action, Action::Adapter { .. }));
    let first = trace.observations.clone();
    for o in &mut trace.observations {
        if let Action::Adapter { observation } = &mut o.action
            && matches!(observation.event, AdapterEvent::Finished { .. })
        {
            observation.event = AdapterEvent::Finished {
                ending: AdapterEnding::Error {
                    boundary: rig::observe::AdapterErrorBoundary::ProviderResponse,
                    kind: "http".into(),
                    status: Some(503),
                    retryable: true,
                },
            };
        }
    }
    for mut o in first {
        let Action::Adapter { observation } = &mut o.action else {
            unreachable!()
        };
        observation.attempt = Some(2);
        // No usage report on the successful retry: unknown, never copied from attempt 1.
        if !matches!(observation.event, AdapterEvent::Usage { .. }) {
            trace.observations.push(o);
        }
    }
    let digest = observed(&serde_json::to_string(&trace).unwrap());
    assert_eq!(digest.attempts.len(), 2);
    assert_eq!(
        digest.attempts[0].usage.as_ref().unwrap().total_tokens,
        Some(856)
    );
    assert!(matches!(
        digest.attempts[0].ending,
        Some(AdapterEnding::Error { .. })
    ));
    assert_eq!(digest.attempts[1].usage, None);
    assert_eq!(digest.attempts[1].ending, Some(AdapterEnding::Terminal));

    let last_usage = trace
        .observations
        .iter_mut()
        .rev()
        .find_map(|o| {
            let Action::Adapter { observation } = &mut o.action else {
                return None;
            };
            let AdapterEvent::Usage { usage } = &mut observation.event else {
                return None;
            };
            Some(usage)
        })
        .unwrap();
    *last_usage = AdapterUsage {
        output_tokens: Some(0),
        ..AdapterUsage::default()
    };
    let digest = observed(&serde_json::to_string(&trace).unwrap());
    let latest = digest.attempts[0].usage.as_ref().unwrap();
    assert_eq!(
        latest.total_tokens, None,
        "a later unknown count is not filled from an older snapshot"
    );
    assert_eq!(latest.output_tokens, Some(0));
}
