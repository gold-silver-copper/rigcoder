use super::*;

#[test]
fn setup_failures_are_observed_once_and_do_not_invent_a_run() {
    use rig::observe::HostAction as _;
    let root = tempfile::tempdir().unwrap();
    let mut baseline = None;
    for enabled in [false, true] {
        let mut app = bevy_app::App::new();
        app.add_plugins(crate::RigcoderPlugin {
            workspace: root.path().to_owned(),
            model: crate::ModelChoice::parse("gemini", None).unwrap(),
            max_turns: 4,
            mode: crate::Mode::Replay(crate::EffectLog::default().into()),
            prompt_override: None,
            keep_stream_events: false,
        });
        if !enabled {
            app.world_mut()
                .remove_resource::<rig_ecs::bus::Witnessing>();
        }
        app.update();
        let transcript: Vec<_> = app
            .world()
            .resource::<crate::Transcript>()
            .events
            .iter()
            .filter_map(|event| match event {
                crate::Event::Failed { reason } => Some(reason.clone()),
                _ => None,
            })
            .collect();
        assert!(!transcript.is_empty());
        assert!(transcript.iter().all(|failure| failure.is_replay_failure()));
        let trace = crate::observations(app.world()).unwrap();
        let observed: Vec<_> = trace
            .observations
            .iter()
            .filter_map(|observation| {
                let failure = FailureDetail::from_action(&observation.action)?;
                assert!(observation.subject.scope.is_none());
                assert!(observation.subject.effect.is_none());
                Some(failure.unwrap())
            })
            .collect();
        if enabled {
            assert_eq!(observed, transcript);
            assert_eq!(baseline.as_ref(), Some(&transcript));
        } else {
            assert!(observed.is_empty());
            baseline = Some(transcript);
        }
        assert!(
            observed
                .iter()
                .all(|failure| failure.adapter.is_none() && failure.http_status.is_none())
        );
        assert!(crate::effect_log(app.world()).records.is_empty());
    }
}

fn retry_trace() -> ObservationTrace {
    serde_json::from_str(include_str!(
        "../../../../fixtures/evidence/gemini/observe_failures/rate_limited_unary/observations.json"
    ))
    .unwrap()
}

#[test]
fn failure_joins_the_failed_scope_and_keeps_report_retryability() {
    let report =
        ErrorReport::new(ErrorKind::Http { status: Some(429) }, "quota").with_retryable(true);
    let mut failure = FailureDetail::report("provider", &report, &[]);
    failure.attach("rigcoder/run/1", &retry_trace());
    let attempt = failure.adapter.as_ref().unwrap();
    assert_eq!(attempt.operation, "rigcoder/request/1/completion/0");
    assert_eq!(attempt.attempt, 1);
    assert_eq!(attempt.host_attempt.unwrap().get(), 1);
    assert_eq!(attempt.response_status, Some(429));
    assert_eq!(failure.retryable, Some(true));
    assert_eq!(failure.boundary, FailureBoundary::ProviderResponse);
    assert!(matches!(
        attempt.ending,
        Some(AdapterEnding::Error {
            status: Some(429),
            ..
        })
    ));
    let encoded = serde_json::to_string(&failure).unwrap();
    assert_eq!(
        serde_json::from_str::<FailureDetail>(&encoded).unwrap(),
        failure
    );

    failure.attach("missing-run", &retry_trace());
    assert!(
        failure.adapter.is_none(),
        "reattachment cannot retain stale identity"
    );
}

#[test]
fn interleaved_sends_do_not_invent_causal_attribution() {
    let mut trace = retry_trace();
    let mut other = trace
        .observations
        .iter()
        .find(|o| {
            o.subject.scope.as_deref() == Some("rigcoder/run/1")
                && matches!(&o.action, Action::Adapter { .. })
        })
        .unwrap()
        .clone();
    if let Action::Adapter { observation } = &mut other.action {
        observation.attempt = Some(2);
    }
    trace.observations.insert(2, other);
    let mut failure = FailureDetail::report(
        "provider",
        &ErrorReport::new(ErrorKind::Http { status: Some(429) }, "quota"),
        &[],
    );
    failure.attach("rigcoder/run/1", &trace);
    assert!(failure.adapter.is_none());
    assert_eq!(failure.kind, "http");
}

#[test]
fn multiple_failed_effects_or_dropped_facts_leave_attribution_unknown() {
    let original = retry_trace();
    let report = ErrorReport::new(ErrorKind::Http { status: Some(429) }, "quota");
    let mut trace = original.clone();
    let mut other = trace
        .observations
        .iter()
        .find(|o| {
            o.subject.scope.as_deref() == Some("rigcoder/run/1")
                && matches!(
                    &o.action,
                    Action::Landed {
                        outcome: rig::observe::OutcomeSummary::Err { .. }
                    }
                )
        })
        .unwrap()
        .clone();
    other.subject.effect = Some(rig::effect::EffectId::from_raw(999));
    trace.observations.push(other);
    let mut failure = FailureDetail::report("provider", &report, &[]);
    failure.attach("rigcoder/run/1", &trace);
    assert!(failure.adapter.is_none());

    let mut trace = original;
    trace.dropped = 1;
    failure.attach("rigcoder/run/1", &trace);
    assert!(failure.adapter.is_none());
}

#[test]
fn classification_uses_types_and_messages_are_bounded_and_redacted() {
    let content_type = rig::http_client::Error::InvalidContentType("text/plain".parse().unwrap());
    let report = ErrorReport::from(rig::completion::CompletionError::HttpError(content_type));
    assert_eq!(report.kind, ErrorKind::Http { status: None });
    assert_eq!(
        FailureDetail::report("provider", &report, &[]).boundary,
        FailureBoundary::Unknown
    );
    for (kind, boundary) in [
        (ErrorKind::Http { status: None }, FailureBoundary::Unknown),
        (ErrorKind::Response, FailureBoundary::Decode),
        (ErrorKind::Json, FailureBoundary::Unknown),
        (ErrorKind::Provider, FailureBoundary::Unknown),
        (ErrorKind::Denied, FailureBoundary::Host),
    ] {
        let failure = FailureDetail::report(
            "provider",
            &ErrorReport::new(kind, "replay recorded divergence"),
            &[],
        );
        assert_eq!(failure.boundary, boundary);
        assert!(
            !failure.is_replay_failure(),
            "message text cannot select replay exit code"
        );
    }
    let replay = FailureDetail::report(
        "provider",
        &ErrorReport::new(ErrorKind::Divergence, "different request"),
        &[],
    );
    assert!(replay.is_replay_failure());
    let secret = "synthetic-credential-123".to_owned();
    let failure = FailureDetail::host(
        "cancelled",
        &format!("operator sent {secret}"),
        std::slice::from_ref(&secret),
    );
    assert!(!serde_json::to_string(&failure).unwrap().contains(&secret));
    assert!(
        FailureDetail::host("cancelled", &"x".repeat(2048), &[])
            .message
            .len()
            <= 512
    );
    let unknown = FailureDetail::runtime(None, &[]);
    assert_eq!(unknown.boundary, FailureBoundary::Unknown);
    assert_eq!(unknown.retryable, None);
    assert_eq!(unknown.http_status, None);
    assert!(unknown.adapter.is_none());
}
