//! The verification matrix for the witness, on the real product session:
//! each cell runs `RigcoderPlugin` with a scripted model (unary or
//! streamed), checks an independent oracle (transcript, files, run state),
//! and then asserts what the observation trace says about the decisions
//! that produced it.
//!
//! | cell | oracle | trace |
//! |---|---|---|
//! | text answer, unary and streamed | `Settled` | `Issued`, `Landed`, `Ended { settled }` |
//! | one bash call, auto approval | file written | `Approval { prepared, approved }`, tool `Landed` |
//! | four calls under concurrency 1 and 4 | all results in history | `Held`/`Released` counts |
//! | invalid tool | run failed | `InvalidCall { fail }`, `Ended { unknown_tool_call }` |
//! | denied bash | `Denied` in transcript, file untouched | `SteerDenial`, `Denied` at `Gate` |
//! | approval asked and refused | file untouched | `Approval { held }` then `{ denied }` |
//! | cancel with bash in flight | run failed cancelled | `CancelRequested`, `Ended { cancelled }` |
//! | provider error, not transient | run failed | `Ended { provider }`, no retry |
//! | transient error then answer | settled, two requests | `ProviderRetry`, two `Ended` |
//! | stream without terminal record | retried then settled | `StreamTruncated` with tail |
//! | model-call budget | run failed | `Ended { max_turns }` |
//! | oversized tool result | history cut | `ResultShaped` |

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use bevy_app::{App, PreStartup};
use bevy_ecs::prelude::*;
use rig::{
    completion::{CompletionResponse, ModelRef, ProviderCapabilities},
    effect::{EffectKind, FamilyDescriptor, HandlerDescriptor, HandlerKey, Outcome},
    error::{ErrorKind, ErrorReport},
    message::AssistantContent,
    observe::{Action, HostAction, ObservationTrace, Stage},
    serve::{Dispatch, Reply, Serve},
    streaming::StreamFinal,
};
use rig_ecs::bus::Handlers;
use rigcoder::{Conversation, Event, ModelChoice, RigcoderPlugin, Transcript};

/// What a scripted turn answers.
enum Answer {
    Text(String),
    Calls(Vec<AssistantContent>),
    Err(ErrorReport),
    /// A stream that writes this text and ends without its terminal record.
    Truncated(String),
}

struct Scripted(Mutex<VecDeque<Answer>>);

impl Serve for Scripted {
    type Family = rig::effect::family::Completion;

    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor {
            key: HandlerKey::from(rigcoder::model::MODEL_KEY),
            family: FamilyDescriptor::Completion {
                model: ModelRef::new("scripted"),
                capabilities: ProviderCapabilities::default(),
            },
            layers: Vec::new(),
        }
    }

    async fn serve(&self, kind: EffectKind, _: Dispatch) -> Reply {
        let streaming = matches!(kind, EffectKind::Completion { stream: true, .. });
        let answer = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Answer::Text("done".into()));
        match answer {
            Answer::Truncated(text) => Reply::written(|mut out| async move {
                let _ = out.text(text).await;
            }),
            Answer::Text(text) if streaming => Reply::written(|mut out| async move {
                let _ = out.text(text).await;
                let _ = out
                    .finish(StreamFinal::new("scripted", rig::completion::Usage::new()))
                    .await;
            }),
            Answer::Text(text) => Reply::Outcome(Ok(Outcome::Completion(CompletionResponse::new(
                vec![AssistantContent::text(text)],
                rig::completion::Usage::new(),
                "scripted",
            )))),
            Answer::Calls(calls) => Reply::Outcome(Ok(Outcome::Completion(
                CompletionResponse::new(calls, rig::completion::Usage::new(), "scripted"),
            ))),
            Answer::Err(report) => Reply::Outcome(Err(report)),
        }
    }
}

#[derive(Resource)]
struct Model(Mutex<Option<Scripted>>);

fn register(mut handlers: Handlers, model: Res<Model>) {
    if let Some(model) = model.0.lock().unwrap().take() {
        handlers
            .register(rigcoder::model::MODEL_KEY, model)
            .unwrap();
    }
}

struct Cell {
    dir: PathBuf,
    app: App,
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rigcoder-observe-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cell(name: &str, answers: Vec<Answer>, max_turns: usize) -> Cell {
    let dir = scratch(name);
    let mut app = App::new();
    app.add_plugins(RigcoderPlugin {
        workspace: dir.clone(),
        model: ModelChoice::parse("gemini", None).unwrap(),
        max_turns,
        mode: rigcoder::Mode::Live,
        prompt_override: Some("You are a test agent.".into()),
        keep_stream_events: false,
    });
    app.insert_resource(Model(Mutex::new(Some(Scripted(Mutex::new(
        answers.into(),
    ))))))
    .add_systems(PreStartup, register);
    Cell { dir, app }
}

impl Cell {
    fn settings(&mut self, stream: bool, retries: u8) {
        self.app.insert_resource(rigcoder::RunSettings {
            stream,
            max_tokens: 1024,
            provider_retries: retries,
        });
    }

    fn start(&mut self, prompt: &str) -> Entity {
        self.app.update();
        rigcoder::submit(self.app.world_mut(), prompt).expect("a run starts")
    }

    fn drive(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            self.app.update();
            self.app
                .world_mut()
                .resource_mut::<Conversation>()
                .expire_backoff();
            if !self.app.world().resource::<Conversation>().is_busy() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the run did not end: {:?}", self.events());
    }

    fn drive_until(&mut self, what: &str, mut done: impl FnMut(&World) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            self.app.update();
            if done(self.app.world()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("{what}: not reached; {:?}", self.events());
    }

    fn events(&self) -> Vec<Event> {
        self.app.world().resource::<Transcript>().events.clone()
    }

    fn ending(&self) -> String {
        self.events()
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Settled { .. } => Some("settled".to_owned()),
                Event::Failed { reason } => Some(reason.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "none".into())
    }

    fn trace(&self) -> ObservationTrace {
        let trace = rigcoder::observations(self.app.world()).expect("a witness is installed");
        // Every trace is a serializable artifact.
        let json = serde_json::to_string(&trace).unwrap();
        let back: ObservationTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(back, trace);
        assert!(trace.is_complete());
        trace
    }

    /// The facts, named: library actions by their variant, host facts by kind.
    fn facts(&self) -> Vec<String> {
        self.trace()
            .observations
            .iter()
            .map(|o| match &o.action {
                Action::Host { kind, payload } => match payload.get("decision") {
                    Some(decision) => format!("{kind}:{}", decision.as_str().unwrap_or("")),
                    None => kind.clone(),
                },
                Action::Ended { ending } => format!("ended:{}", ending.code),
                Action::Denied { reason } => format!("denied@{:?}:{}", o.stage, reason.code),
                Action::Refused { reason } => format!("refused:{}", reason.code),
                Action::Held { .. } => "held".into(),
                Action::Released => "released".into(),
                Action::Issued => "issued".into(),
                Action::Landed { .. } => "landed".into(),
                Action::StreamTruncated { .. } => "stream_truncated".into(),
                Action::Replaced { .. } => "replaced".into(),
                Action::Cancelled { .. } => "cancelled".into(),
                Action::CancelRequested { .. } => "cancel_requested".into(),
                Action::Retry { .. } => "retry".into(),
                Action::InvalidCall { .. } => "invalid_call".into(),
                Action::Approved { .. } => "approved".into(),
                Action::Patched { .. } => "patched".into(),
                Action::Deferred { .. } => "deferred".into(),
            })
            .collect()
    }

    fn count(&self, fact: &str) -> usize {
        self.facts().iter().filter(|f| f.as_str() == fact).count()
    }
}

fn call(id: &str, name: &str, args: serde_json::Value) -> AssistantContent {
    AssistantContent::tool_call(id, name, args)
}

fn bash(id: &str, command: &str) -> AssistantContent {
    call(id, "bash", serde_json::json!({"command": command}))
}

fn text(s: &str) -> Answer {
    Answer::Text(s.into())
}

fn transient() -> Answer {
    Answer::Err(ErrorReport::new(ErrorKind::Timeout, "timed out"))
}

#[test]
fn a_text_answer_settles_unary_and_streamed() {
    for stream in [false, true] {
        let mut cell = cell(&format!("text-{stream}"), vec![text("hello")], 4);
        cell.settings(stream, 0);
        cell.start("say hello");
        cell.drive();
        assert_eq!(cell.ending(), "settled");
        let facts = cell.facts();
        assert_eq!(
            facts,
            ["issued", "landed", "ended:settled"],
            "stream={stream}: {facts:?}"
        );
        let trace = cell.trace();
        assert!(
            trace
                .observations
                .iter()
                .all(|o| o.subject.scope.as_deref() == Some("rigcoder/run/1")),
            "every fact carries the run's scope: {trace:?}"
        );
    }
}

#[test]
fn one_bash_call_is_prepared_approved_and_lands() {
    let mut cell = cell(
        "one-tool",
        vec![
            Answer::Calls(vec![bash("c1", "printf hi > out.txt")]),
            text("written"),
        ],
        4,
    );
    cell.settings(false, 0);
    cell.start("write out.txt");
    cell.drive();
    assert_eq!(
        std::fs::read_to_string(cell.dir.join("out.txt")).unwrap(),
        "hi"
    );
    // The approval gate holds the call while it prepares (a bus `Held`
    // the witness sees), the runtime's batch release lifts that hold each
    // pass, and the gate's own facts name the decision.
    let facts = cell.facts();
    assert_eq!(
        facts,
        [
            "issued",
            "landed",
            "held",
            "released",
            "rigcoder/approval:prepared",
            "rigcoder/approval:approved",
            "issued",
            "landed",
            "issued",
            "landed",
            "ended:settled"
        ],
        "{facts:?}"
    );
    let approved = cell
        .trace()
        .observations
        .into_iter()
        .find(|o| matches!(&o.action, Action::Host { payload, .. } if payload["decision"] == "approved"))
        .unwrap();
    assert_eq!(approved.stage, Stage::Host);
    assert_eq!(approved.emitter.name, "rigcoder/approval");
    let fact = rigcoder::observe::Approval::from_action(&approved.action)
        .unwrap()
        .unwrap();
    assert_eq!(fact.tool, "bash");
    assert_eq!(fact.mode, "auto");
    assert!(!fact.operation.is_empty());
}

#[test]
fn a_batch_beyond_the_concurrency_is_held_and_released_in_order() {
    let mut holds = Vec::new();
    for concurrency in [1usize, 4] {
        let mut cell = cell(
            &format!("batch-{concurrency}"),
            vec![
                Answer::Calls(
                    (1..=4)
                        .map(|i| bash(&format!("c{i}"), &format!("echo {i}")))
                        .collect(),
                ),
                text("all four ran"),
            ],
            4,
        );
        cell.settings(false, 0);
        cell.app.update();
        let agent = cell.app.world().resource::<rigcoder::AgentHandle>().agent;
        cell.app
            .world_mut()
            .entity_mut(agent)
            .insert(rig_ecs::agent::ToolPolicy { concurrency });
        rigcoder::submit(cell.app.world_mut(), "run four").unwrap();
        cell.drive();
        assert_eq!(cell.ending(), "settled");
        let results = cell
            .events()
            .iter()
            .filter(|e| matches!(e, Event::ToolResult { ok: true, .. }))
            .count();
        assert_eq!(results, 4);
        // Every call is held at least while its approval prepares; under
        // concurrency 1 the batch's own holds are on top of that, released
        // one at a time as the previous call lands.
        let held = cell.count("held");
        assert!(held >= 4, "concurrency {concurrency}: {:?}", cell.facts());
        assert_eq!(cell.count("released"), held, "every hold is released once");
        assert_eq!(cell.count("rigcoder/approval:approved"), 4);
        holds.push(held);
    }
    assert!(
        holds[0] > holds[1],
        "concurrency 1 holds more than 4: {holds:?}"
    );
}

#[test]
fn an_invalid_tool_call_fails_the_run_with_its_resolution() {
    let mut cell = cell(
        "invalid",
        vec![Answer::Calls(vec![call(
            "c1",
            "teleport",
            serde_json::json!({}),
        )])],
        4,
    );
    cell.settings(false, 0);
    cell.start("go");
    cell.drive();
    assert!(
        cell.ending().contains("UnknownToolCall"),
        "{}",
        cell.ending()
    );
    let facts = cell.facts();
    assert!(facts.contains(&"invalid_call".to_owned()), "{facts:?}");
    assert_eq!(
        facts.last().map(String::as_str),
        Some("ended:unknown_tool_call")
    );
}

#[test]
fn a_denied_bash_command_is_a_steer_fact_and_a_gate_denial() {
    let mut cell = cell(
        "deny",
        vec![
            Answer::Calls(vec![bash("c1", "rm -rf /")]),
            text("I could not"),
        ],
        4,
    );
    cell.settings(false, 0);
    cell.start("clean up");
    cell.drive();
    assert!(
        cell.events()
            .iter()
            .any(|e| matches!(e, Event::Denied { name, .. } if name == "bash"))
    );
    let facts = cell.facts();
    assert!(facts.contains(&"rigcoder/steer".to_owned()), "{facts:?}");
    assert!(
        facts.contains(&"denied@Gate:denied".to_owned()),
        "{facts:?}"
    );
    assert!(
        !facts.contains(&"rigcoder/approval:prepared".to_owned()),
        "never prepared: {facts:?}"
    );
    let steer = cell
        .trace()
        .observations
        .into_iter()
        .find(|o| matches!(&o.action, Action::Host { kind, .. } if kind == "rigcoder/steer"))
        .unwrap();
    let fact = rigcoder::observe::SteerDenial::from_action(&steer.action)
        .unwrap()
        .unwrap();
    assert_eq!(fact.rule, "deny");
    assert!(fact.reason.contains("refusing to delete /"));
    assert_eq!(steer.subject.scope.as_deref(), Some("rigcoder/run/1"));
}

#[test]
fn an_approval_asked_and_refused_is_held_then_denied() {
    let mut cell = cell(
        "ask-deny",
        vec![
            Answer::Calls(vec![bash("c1", "printf no > out.txt")]),
            text("understood"),
        ],
        4,
    );
    cell.settings(false, 0);
    cell.app
        .insert_resource(rigcoder::approval::ApprovalMode::Ask);
    cell.start("write a file");
    cell.drive_until("held", |world| {
        !world
            .resource::<rigcoder::approval::Approvals>()
            .pending
            .is_empty()
    });
    cell.app
        .world_mut()
        .resource_mut::<rigcoder::approval::Approvals>()
        .deny_next();
    cell.drive();
    assert!(
        !cell.dir.join("out.txt").exists(),
        "the reviewer's no is a no"
    );
    let facts = cell.facts();
    let approvals: Vec<&String> = facts
        .iter()
        .filter(|f| f.starts_with("rigcoder/approval"))
        .collect();
    assert_eq!(
        approvals,
        ["rigcoder/approval:held", "rigcoder/approval:denied"],
        "{facts:?}"
    );
    assert!(
        facts.contains(&"held".to_owned()),
        "the bus saw the hold too: {facts:?}"
    );
    assert!(
        facts.contains(&"denied@Gate:denied".to_owned()),
        "{facts:?}"
    );
    assert_eq!(cell.ending(), "settled");
}

#[test]
fn cancelling_with_bash_in_flight_is_requested_then_ends_cancelled() {
    let mut cell = cell(
        "cancel",
        vec![Answer::Calls(vec![bash(
            "c1",
            "sleep 5; printf late > late.txt",
        )])],
        4,
    );
    cell.settings(false, 0);
    cell.start("wait");
    cell.drive_until("bash in flight", |world| {
        world
            .resource::<Transcript>()
            .events
            .iter()
            .any(|e| matches!(e, Event::ToolCall { .. }))
    });
    cell.app.update();
    rigcoder::cancel(cell.app.world_mut(), "operator stop");
    cell.drive();
    assert!(cell.ending().contains("operator stop"), "{}", cell.ending());
    let facts = cell.facts();
    let ending = facts
        .iter()
        .position(|f| f == "ended:cancelled")
        .expect("ended cancelled");
    let requested = facts
        .iter()
        .position(|f| f == "cancel_requested")
        .expect("requested");
    assert!(requested < ending, "{facts:?}");
    // The bash handler is left to its own cancellation flag and deadline
    // (`tools::rewrite_tests` prove the kill); the run's ending does not
    // wait for it.
}

#[test]
fn a_non_transient_provider_error_ends_the_run_without_a_retry() {
    let mut cell = cell(
        "provider",
        vec![Answer::Err(
            ErrorReport::new(ErrorKind::Provider, "blocked the prompt").with_retryable(false),
        )],
        4,
    );
    cell.settings(false, 3);
    cell.start("hi");
    cell.drive();
    assert!(cell.ending().contains("blocked the prompt"));
    let facts = cell.facts();
    assert_eq!(facts, ["issued", "landed", "ended:provider"], "{facts:?}");
    assert_eq!(cell.count("rigcoder/provider_retry"), 0);
}

#[test]
fn a_transient_provider_error_is_retried_as_a_named_decision() {
    let mut cell = cell("transient", vec![transient(), text("second time")], 4);
    cell.settings(false, 3);
    cell.start("hi");
    cell.drive();
    assert_eq!(cell.ending(), "settled");
    let facts = cell.facts();
    assert_eq!(
        facts,
        [
            "issued",
            "landed",
            "ended:provider",
            "rigcoder/provider_retry",
            "issued",
            "landed",
            "ended:settled"
        ],
        "{facts:?}"
    );
    let retry = cell
        .trace()
        .observations
        .into_iter()
        .find(
            |o| matches!(&o.action, Action::Host { kind, .. } if kind == "rigcoder/provider_retry"),
        )
        .unwrap();
    let fact = rigcoder::observe::ProviderRetry::from_action(&retry.action)
        .unwrap()
        .unwrap();
    assert_eq!(fact.attempt, 1);
    assert_eq!(retry.subject.scope.as_deref(), Some("rigcoder/run/1"));
}

#[test]
fn a_stream_without_its_terminal_record_keeps_its_tail_and_is_retried() {
    let mut cell = cell(
        "truncated",
        vec![
            Answer::Truncated("partial answer".into()),
            text("whole answer"),
        ],
        4,
    );
    cell.settings(true, 3);
    cell.start("hi");
    cell.drive();
    assert_eq!(cell.ending(), "settled");
    let facts = cell.facts();
    assert!(facts.contains(&"stream_truncated".to_owned()), "{facts:?}");
    assert_eq!(cell.count("rigcoder/provider_retry"), 1, "{facts:?}");
    let truncated = cell
        .trace()
        .observations
        .into_iter()
        .find(|o| matches!(o.action, Action::StreamTruncated { .. }))
        .unwrap();
    let Action::StreamTruncated {
        delivered, tail, ..
    } = truncated.action
    else {
        panic!()
    };
    assert!(delivered >= 1);
    assert!(!tail.is_empty(), "the last frames explain the cut");
    assert_eq!(truncated.emitter.name, "rig-ecs/bus");
}

#[test]
fn the_model_call_budget_ends_the_run_as_max_turns() {
    let mut cell = cell(
        "budget",
        vec![
            Answer::Calls(vec![bash("c1", "true")]),
            Answer::Calls(vec![bash("c2", "true")]),
            Answer::Calls(vec![bash("c3", "true")]),
        ],
        2,
    );
    cell.settings(false, 0);
    cell.start("loop");
    cell.drive();
    assert!(cell.ending().contains("MaxTurns"), "{}", cell.ending());
    assert_eq!(
        cell.facts().last().map(String::as_str),
        Some("ended:max_turns")
    );
}

#[test]
fn an_oversized_tool_result_is_shaped_as_a_named_judge_decision() {
    // Bash output is captured to a bounded head and tail, so a file read
    // is what can exceed the history budget.
    let mut cell = cell(
        "shaped",
        vec![
            Answer::Calls(vec![call(
                "c1",
                "read_file",
                serde_json::json!({"path": "big.txt"}),
            )]),
            text("big"),
        ],
        4,
    );
    std::fs::write(cell.dir.join("big.txt"), "a".repeat(40_000)).unwrap();
    cell.settings(false, 0);
    cell.start("read the big file");
    cell.drive();
    assert_eq!(cell.ending(), "settled");
    assert_eq!(
        cell.count("rigcoder/result_shaped"),
        1,
        "{:?}",
        cell.facts()
    );
    let shaped = cell
        .trace()
        .observations
        .into_iter()
        .find(
            |o| matches!(&o.action, Action::Host { kind, .. } if kind == "rigcoder/result_shaped"),
        )
        .unwrap();
    let fact = rigcoder::observe::ResultShaped::from_action(&shaped.action)
        .unwrap()
        .unwrap();
    assert!(fact.chars > fact.kept);
    assert_eq!(fact.tool, "read_file");
}

#[test]
fn a_witness_trace_written_by_the_cli_shape_is_readable_by_the_digest() {
    // The bench reads `observations.json` beside the transcript; the shape
    // it counts is the one the product writes.
    let mut cell = cell("digest", vec![transient(), text("ok")], 4);
    cell.settings(false, 3);
    cell.start("hi");
    cell.drive();
    let json = serde_json::to_string(&cell.trace()).unwrap();
    let dir = cell.dir.join("agent");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("observations.json"), &json).unwrap();
    assert!(Path::new(&dir).join("observations.json").is_file());
    assert!(json.contains("\"kind\":\"rigcoder/provider_retry\""));
}
