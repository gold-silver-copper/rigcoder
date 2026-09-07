//! The steering systems, pinned the way rig-ecs pins its hook rows: a
//! scripted model, a run, and the transcript and requests that result.

use std::sync::{Arc, Mutex};

use bevy_app::{App, AppExit, PostStartup, PreStartup, ScheduleRunnerPlugin};
use bevy_ecs::prelude::*;
use rig::{
    completion::{CompletionResponse, ModelRef, ProviderCapabilities, Usage},
    effect::{EffectKind, FamilyDescriptor, HandlerDescriptor, HandlerKey, Outcome},
    message::AssistantContent,
    serve::{OutcomeSink, Serve},
};
use rig_ecs::{agent::MessageParts, bus::Handlers};
use rigcoder::{Event, ModelChoice, RigcoderPlugin, Transcript, steer::Steer};

/// A model answering each request with the next scripted turn, keeping
/// every request it saw.
struct Scripted {
    turns: Mutex<std::collections::VecDeque<Vec<AssistantContent>>>,
    seen: Arc<Mutex<Vec<Vec<MessageParts>>>>,
}

impl Serve for Scripted {
    type Family = rig::effect::family::Completion;

    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor {
            key: HandlerKey::from(rigcoder::model::MODEL_KEY),
            family: FamilyDescriptor::Completion { model: ModelRef::new("scripted"), capabilities: ProviderCapabilities::default() },
            layers: Vec::new(),
        }
    }

    async fn serve(&self, kind: EffectKind, sink: OutcomeSink) {
        if let EffectKind::Completion { request, .. } = &kind {
            self.seen.lock().unwrap().push(request.chat_history.iter().filter_map(MessageParts::from_message).collect());
        }
        let next = self.turns.lock().unwrap().pop_front().unwrap_or_else(|| vec![AssistantContent::text("(script over)")]);
        sink.resolve(Ok(Outcome::Completion(CompletionResponse::new(next, Usage::new(), "scripted")))).await;
    }
}

#[derive(Resource)]
struct ScriptedModel(Mutex<Option<Scripted>>);

/// The transcript, copied out once the run is over: the world is not
/// readable after `App::run` returns under the schedule runner.
#[derive(Resource)]
struct Captured(Arc<Mutex<Vec<Event>>>);

fn capture_when_over(conversation: Res<rigcoder::Conversation>, transcript: Res<Transcript>, captured: Res<Captured>, mut exit: MessageWriter<AppExit>) {
    if conversation.runs > 0 && conversation.active.is_none() {
        *captured.0.lock().unwrap() = transcript.events.clone();
        exit.write(AppExit::Success);
    }
}

fn register_scripted(mut handlers: Handlers, model: Res<ScriptedModel>) {
    if let Some(model) = model.0.lock().unwrap().take() {
        handlers.register(rigcoder::model::MODEL_KEY, model).expect("a fresh key");
    }
}

fn call(name: &str, args: serde_json::Value) -> AssistantContent {
    AssistantContent::tool_call(format!("call-{name}-{args}"), name, args)
}

/// Run `prompt` against a scripted model with the given steering rules;
/// returns the transcript and every request the model saw.
fn run_scripted(workspace: &std::path::Path, steer: Steer, script: Vec<Vec<AssistantContent>>, prompt: &'static str) -> (Vec<Event>, Vec<Vec<MessageParts>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let model = Scripted { turns: Mutex::new(script.into()), seen: seen.clone() };
    let mut app = App::new();
    app.add_plugins((
        ScheduleRunnerPlugin::run_loop(std::time::Duration::from_millis(1)),
        RigcoderPlugin { workspace: workspace.to_path_buf(), model: ModelChoice::parse("gemini", None).unwrap(), max_turns: 8 },
    ))
    .insert_resource(steer)
    .insert_resource(ScriptedModel(Mutex::new(Some(model))))
    .add_systems(PreStartup, register_scripted)
    .add_systems(PostStartup, move |world: &mut World| {
        rigcoder::submit(world, prompt);
    })
    .insert_resource(Captured(captured.clone()))
    .add_systems(bevy_app::Last, capture_when_over);
    app.run();
    let events = captured.lock().unwrap().clone();
    let seen = seen.lock().unwrap().clone();
    (events, seen)
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rigcoder-steer-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_denied_bash_command_reaches_the_model_as_a_denial_and_never_runs() {
    let dir = scratch("deny");
    let (events, seen) = run_scripted(
        &dir,
        Steer::default(),
        vec![vec![call("bash", serde_json::json!({"command": "find / -name report.jsonl"}))], vec![AssistantContent::text("ok")]],
        "look around",
    );
    assert!(events.iter().any(|e| matches!(e, Event::Denied { name, .. } if name == "bash")), "{events:?}");
    // The denial is what the model reads as the tool's result; nothing ran.
    assert!(events.iter().any(|e| matches!(e, Event::ToolResult { ok: false, output, .. } if output.starts_with("denied:"))), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::ToolResult { ok: true, .. })), "the tool never ran: {events:?}");
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Assistant { .. })).count(), 1, "the answer is shown once: {events:?}");
    let second = format!("{:?}", seen[1]);
    assert!(second.contains("denied") && second.contains("workspace"), "{second}");
}

#[test]
fn an_over_long_result_is_cut_for_history_and_the_run_goes_on() {
    let dir = scratch("shape");
    let steer = Steer { max_result_chars: 200, ..Default::default() };
    let (events, seen) = run_scripted(
        &dir,
        steer,
        vec![vec![call("bash", serde_json::json!({"command": "yes line | head -n 300"}))], vec![AssistantContent::text("done")]],
        "print a lot",
    );
    assert!(events.iter().any(|e| matches!(e, Event::Settled { .. })), "{events:?}");
    let second = format!("{:?}", seen[1]);
    assert!(second.contains("result cut to 200 chars"), "{second}");
}

#[test]
fn a_text_only_answer_with_a_deliverable_missing_is_retried_then_accepted() {
    let dir = scratch("deliverable");
    let report = dir.join("report.jsonl");
    let steer = Steer { deliverables: vec![report.clone()], ..Default::default() };
    let (events, seen) = run_scripted(
        &dir,
        steer,
        vec![
            vec![AssistantContent::text("I think the answer is cwe-93. Should I write it?")],
            vec![call("write_file", serde_json::json!({"path": report.display().to_string(), "content": "{\"cwe_id\": [\"cwe-93\"]}\n"}))],
            vec![AssistantContent::text("Wrote the report.")],
        ],
        "write the report",
    );
    assert!(events.iter().any(|e| matches!(e, Event::Settled { answer } if answer == "Wrote the report.")), "{events:?}");
    assert!(report.is_file());
    let second = format!("{:?}", seen[1]);
    assert!(second.contains("Not finished") && second.contains("report.jsonl"), "{second}");
    assert_eq!(seen.len(), 3);
}
