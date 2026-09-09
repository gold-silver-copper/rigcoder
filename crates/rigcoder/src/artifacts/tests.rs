use super::*;

const SECRET: &str = "synthetic-private-diagnostic-value";

#[test]
fn endpoint_component_echoes_are_scrubbed_by_product_failure_and_trace_exports() {
    let mut world = bevy_ecs::world::World::new();
    world.init_resource::<crate::Transcript>();
    world.insert_resource(crate::model::ModelConnection::new(
        "https://opaque%2Dname:opaque%2Bpassword@example.invalid/v1?API_KEY=query%2Dvalue",
        "configured-\nvalue",
        rig::http_client::ReqwestClient::new(reqwest::Client::new()).boxed(),
    ));
    let log = std::sync::Arc::new(rig::observe::ObservationLog::default());
    rig_ecs::bus::Witnessing::install(&mut world, log.clone());
    world.insert_resource(crate::observe::Observations(log.clone()));
    for echo in [
        "opaque-name",
        "opaque%2dname",
        "opaque+password",
        "opaque%2Bpassword",
        "query-value",
        "query%2Dvalue",
        "configured-value",
    ] {
        let report = ErrorReport::new(rig::error::ErrorKind::Internal, echo);
        crate::failure::record_host_report(&mut world, "checkpoint", &report);
        world.resource::<rig_ecs::bus::Witnessing>().emit(
            rig::observe::Subject::default(),
            rig::observe::Stage::Host,
            crate::observe::emitter("test"),
            Action::CancelRequested {
                reason: Reason::with_detail("operator", echo),
            },
        );
        assert!(serde_json::to_string(&log.trace()).unwrap().contains(echo));
        assert!(
            !serde_json::to_string(&crate::observe::artifact(&world).unwrap())
                .unwrap()
                .contains(echo)
        );
        assert!(
            !serde_json::to_string(&world.resource::<crate::Transcript>().events)
                .unwrap()
                .contains(echo)
        );
        assert_eq!(
            report.message, echo,
            "runtime errors keep their original values"
        );
    }
}

#[test]
fn startup_credentials_still_scrub_exports_after_connection_replacement_or_removal() {
    let root = tempfile::tempdir().unwrap();
    let mut app = bevy_app::App::new();
    app.add_plugins(crate::RigcoderPlugin {
        workspace: root.path().to_owned(),
        model: crate::ModelChoice::parse("gemini", None).unwrap(),
        max_turns: 4,
        mode: crate::Mode::Replay(crate::EffectLog::default().into()),
        prompt_override: None,
        keep_stream_events: false,
    });
    let connection = |key: &str| {
        crate::model::ModelConnection::new(
            "https://example.invalid",
            key,
            rig::http_client::ReqwestClient::new(reqwest::Client::new()).boxed(),
        )
    };
    app.insert_resource(connection(SECRET));
    app.update(); // Product startup captures credentials before model setup.
    for replacement in [Some(connection("replacement-synthetic-value")), None] {
        app.world_mut()
            .remove_resource::<crate::model::ModelConnection>();
        if let Some(connection) = replacement {
            app.insert_resource(connection);
        }
        crate::failure::record_host_report(app.world_mut(), "checkpoint", &failure());
        let witness = app.world().resource::<rig_ecs::bus::Witnessing>();
        witness.emit(
            rig::observe::Subject::default(),
            rig::observe::Stage::Host,
            crate::observe::emitter("test"),
            Action::CancelRequested {
                reason: Reason::with_detail("operator", SECRET),
            },
        );
        let raw = app
            .world()
            .resource::<crate::observe::Observations>()
            .0
            .trace();
        assert!(serde_json::to_string(&raw).unwrap().contains(SECRET));
        let artifact = crate::observe::artifact(app.world()).unwrap();
        assert!(!serde_json::to_string(&artifact).unwrap().contains(SECRET));
        let transcript = app.world().resource::<crate::Transcript>();
        assert!(
            !serde_json::to_string(&transcript.events)
                .unwrap()
                .contains(SECRET)
        );
        assert!(
            !format!(
                "{:?}",
                app.world().resource::<crate::Setup>().diagnostic_secrets
            )
            .contains(SECRET)
        );
    }
}

#[test]
fn environment_credentials_remain_known_when_an_explicit_connection_takes_over() {
    const CHILD: &str = "RIGCODER_TEST_CAPTURED_CREDENTIAL_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Isolate environment selection without mutating this test process's
        // environment while other Rust tests or worker threads may be running.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "artifacts::tests::environment_credentials_remain_known_when_an_explicit_connection_takes_over",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env("GEMINI_API_KEY", SECRET)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated credential regression failed"
        );
        return;
    }
    let mut world = bevy_ecs::world::World::new();
    world.init_resource::<crate::Transcript>();
    world.init_resource::<crate::Setup>();
    world
        .resource_mut::<crate::Setup>()
        .diagnostic_secrets
        .capture(None);
    world.insert_resource(crate::model::ModelConnection::new(
        "https://example.invalid",
        "replacement-value",
        rig::http_client::ReqwestClient::new(reqwest::Client::new()).boxed(),
    ));
    // Current explicit credentials bypass environment lookup. Only the captured
    // startup value can redact this older report after the connection changes.
    crate::failure::record_host_report(&mut world, "checkpoint", &failure());
    let transcript = world.resource::<crate::Transcript>();
    assert!(
        !serde_json::to_string(&transcript.events)
            .unwrap()
            .contains(SECRET)
    );
}

#[test]
fn checkpoint_diagnostics_are_scrubbed_at_each_saved_error_location() {
    use rig_ecs::{
        agent::{
            Failed, Failure,
            scene::{SceneEntity, SceneKind, WorldScene},
        },
        bus::{SceneEffect, Seq, Streamed},
    };
    let log: crate::EffectLog = serde_json::from_str(include_str!(
        "../../../../fixtures/evidence/gemini/observe_failures/bad_key_unary/effects.json"
    ))
    .unwrap();
    let mut saved = WorldScene::default();
    saved.effects.effects.push(SceneEffect {
        seq: Seq(0),
        key: log.records[0].key.clone(),
        kind: log.records[0].kind.clone(),
        id: Some(log.records[0].id),
        outcome: Some(Err(failure())),
        streamed: Some(Streamed {
            errors: vec![(2, failure())],
            events: vec![],
            text: "ordinary progress".into(),
            outcome: Some(Err(failure())),
        }),
        parent: None,
        parent_ref: None,
        scope: Some("run".into()),
        held: false,
        tool_inputs: None,
        tool_outputs: None,
    });
    saved.graph.entities.push(SceneEntity {
        kind: SceneKind::Run,
        parent: None,
        relations: vec![],
        components: serde_json::Map::from_iter([(
            "failed".into(),
            serde_json::to_value(Failed(Failure::Provider(failure()))).unwrap(),
        )]),
    });
    let original = serde_json::to_value(&saved).unwrap();
    scene(&mut saved, &[SECRET.into()]).unwrap();
    assert!(!serde_json::to_string(&saved).unwrap().contains(SECRET));
    let effect = &saved.effects.effects[0];
    assert_eq!(
        serde_json::to_value(&effect.kind).unwrap(),
        original["effects"]["effects"][0]["kind"]
    );
    assert_eq!(effect.id, Some(log.records[0].id));
    let stream = effect.streamed.as_ref().unwrap();
    assert_eq!(stream.text, "ordinary progress");
    assert_eq!(stream.errors[0].0, 2);
    assert_eq!(stream.errors[0].1.http_status, Some(429));
    assert!(stream.errors[0].1.retryable);
    let failed: Failed =
        serde_json::from_value(saved.graph.entities[0].components["failed"].clone()).unwrap();
    assert!(
        matches!(failed.0, Failure::Provider(error) if error.retryable && error.http_status == Some(429))
    );
    saved.graph.entities[0]
        .components
        .insert("failed".into(), serde_json::json!({"unexpected": SECRET}));
    assert!(scene(&mut saved, &[SECRET.into()]).is_err());
}

fn failure() -> ErrorReport {
    let mut error = ErrorReport::new(rig::error::ErrorKind::ProviderResponse, SECRET)
        .with_retryable(true)
        .with_http_status(429)
        .with_code("RESOURCE_EXHAUSTED");
    error.source_chain = vec![SECRET.into(), "x".repeat(2048)];
    error.request_id = Some(SECRET.into());
    error.provider_response = Some(Box::new(
        rig::ProviderResponseError::without_status(SECRET)
            .with_provider_request_id(Some(SECRET.into())),
    ));
    error
}

#[test]
fn exported_errors_are_scrubbed_without_rewriting_exchanges_or_policy() {
    let mut original: crate::EffectLog = serde_json::from_str(include_str!(
        "../../../../fixtures/evidence/gemini/observe_failures/bad_key_unary/effects.json"
    ))
    .unwrap();
    original.records[0].outcome = Err(failure());
    original.header.stream_errors.insert(
        original.records[0].id,
        vec![rig_effect_log::RecordedStreamError {
            item: 3,
            error: failure(),
        }],
    );
    let mut exported = original.clone();
    effect_log(&mut exported, &[SECRET.into()]);
    assert_eq!(
        serde_json::to_value(&exported.records[0].kind).unwrap(),
        serde_json::to_value(&original.records[0].kind).unwrap()
    );
    assert_eq!(exported.records[0].events, original.records[0].events);
    assert_eq!(exported.records[0].id, original.records[0].id);
    assert_eq!(exported.records[0].scope, original.records[0].scope);
    let error = exported.records[0].outcome.as_ref().unwrap_err();
    assert_eq!(error.kind, failure().kind);
    assert!(error.is_retryable());
    assert_eq!(error.http_status, Some(429));
    assert_eq!(error.code.as_deref(), Some("RESOURCE_EXHAUSTED"));
    assert!(error.source_chain.iter().all(|s| s.len() <= 512));
    let stream = &exported.header.stream_errors[&exported.records[0].id][0];
    assert_eq!(stream.item, 3);
    assert_eq!(&stream.error, error);
    let encoded = serde_json::to_string(&exported).unwrap();
    assert!(!encoded.contains(SECRET));
    let decoded: crate::EffectLog = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.records[0].outcome.as_ref().unwrap_err(), error);
    assert_eq!(
        original.records[0].outcome.as_ref().unwrap_err().message,
        SECRET
    );
    // Exporting again is stable: replay of a scrubbed report does not keep changing it.
    let mut second = decoded;
    effect_log(&mut second, &[SECRET.into()]);
    assert_eq!(serde_json::to_string(&second).unwrap(), encoded);
}

#[test]
fn successful_replay_content_is_not_treated_as_diagnostic_text() {
    let mut log: crate::EffectLog = serde_json::from_str(include_str!(
        "../../../../fixtures/evidence/gemini/observe_wire/unary_http_boundary/effects.json"
    ))
    .unwrap();
    assert!(log.records.iter().all(|record| record.outcome.is_ok()));
    // Even an exact match in ordinary program data must not be rewritten by
    // a diagnostic-only exporter. Such data has its own privacy contract.
    let original = serde_json::to_value(&log).unwrap();
    effect_log(&mut log, &["gemini".into(), "text".into()]);
    assert_eq!(serde_json::to_value(log).unwrap(), original);
}

#[test]
fn exported_failure_reasons_are_scrubbed_without_losing_order_or_endings() {
    let mut original: ObservationTrace = serde_json::from_str(include_str!(
        "../../../../fixtures/evidence/gemini/observe_failures/bad_key_unary/observations.json"
    ))
    .unwrap();
    let diagnostic = Reason::with_detail("provider", SECRET);
    let actions = [
        Action::Landed {
            outcome: OutcomeSummary::Err {
                reason: diagnostic.clone(),
                retryable: true,
            },
        },
        Action::Replaced {
            recorded: OutcomeSummary::Err {
                reason: diagnostic.clone(),
                retryable: true,
            },
            consumed: OutcomeSummary::Err {
                reason: diagnostic.clone(),
                retryable: false,
            },
        },
        Action::StreamTruncated {
            delivered: 2,
            tail: vec![],
            errors: vec![diagnostic.clone()],
        },
        Action::Ended { ending: diagnostic },
    ];
    let template = original.observations[0].clone();
    original.observations = actions
        .into_iter()
        .enumerate()
        .map(|(index, action)| {
            let mut observation = template.clone();
            observation.seq = index as u64;
            observation.action = action;
            observation
        })
        .collect();
    let mut exported = original.clone();
    observations(&mut exported, &[SECRET.into()]);
    assert_eq!(exported.observations.len(), original.observations.len());
    for (before, after) in original.observations.iter().zip(&exported.observations) {
        assert_eq!(before.seq, after.seq);
        assert_eq!(before.subject, after.subject);
    }
    assert_eq!(exported.finalized, original.finalized);
    assert_eq!(exported.dropped, original.dropped);
    assert!(
        matches!(&exported.observations[3].action, Action::Ended { ending }
        if ending.code == "provider" && ending.detail.as_deref() != Some(SECRET))
    );
    assert!(!serde_json::to_string(&exported).unwrap().contains(SECRET));
    assert!(serde_json::to_string(&original).unwrap().contains(SECRET));
}
