//! Record, replay, diverge, checkpoint, resume: rig-ecs's log and scene
//! through rigcoder's plugin, with a scripted model standing in for the
//! provider.

use std::sync::{Arc, Mutex};

use bevy_app::{App, AppExit, PostStartup, PreStartup, ScheduleRunnerPlugin};
use bevy_ecs::prelude::*;
use rig::{
    completion::{CompletionResponse, ModelRef, ProviderCapabilities, Usage},
    effect::{EffectKind, FamilyDescriptor, HandlerDescriptor, HandlerKey, Outcome},
    message::AssistantContent,
    serve::{OutcomeSink, Serve},
};
use rig_ecs::{agent::scene::WorldScene, bus::Handlers};
use rigcoder::{EffectLog, Event, Mode, ModelChoice, RigcoderPlugin, Transcript, checkpoint::Checkpoint};

struct Scripted(Mutex<std::collections::VecDeque<Vec<AssistantContent>>>);

impl Serve for Scripted {
    type Family = rig::effect::family::Completion;
    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor {
            key: HandlerKey::from(rigcoder::model::MODEL_KEY),
            family: FamilyDescriptor::Completion { model: ModelRef::new("scripted"), capabilities: ProviderCapabilities::default() },
            layers: Vec::new(),
        }
    }
    async fn serve(&self, _kind: EffectKind, sink: OutcomeSink) {
        let next = self.0.lock().unwrap().pop_front().unwrap_or_else(|| vec![AssistantContent::text("(script over)")]);
        sink.resolve(Ok(Outcome::Completion(CompletionResponse::new(next, Usage::new(), "scripted")))).await;
    }
}

#[derive(Resource)]
struct ScriptedModel(Mutex<Option<Scripted>>);

fn register_scripted(mut handlers: Handlers, model: Res<ScriptedModel>) {
    if let Some(model) = model.0.lock().unwrap().take() {
        handlers.register(rigcoder::model::MODEL_KEY, model).expect("a fresh key");
    }
}

#[derive(Clone, Default)]
struct Outcome_ {
    events: Vec<Event>,
    log: Option<EffectLog>,
}

#[derive(Resource)]
struct Captured(Arc<Mutex<Outcome_>>);

#[derive(Resource, Default)]
struct Ticks(usize);

/// Over when the run ended, or when setup failed (a `Failed` event and no
/// run), or after a tick cap: a test never spins forever.
fn capture_when_over(world: &mut World) {
    let ticks = {
        let mut t = world.get_resource_or_insert_with(Ticks::default);
        t.0 += 1;
        t.0
    };
    let over = {
        let c = world.resource::<rigcoder::Conversation>();
        let t = world.resource::<Transcript>();
        (c.runs > 0 && c.active.is_none()) || (c.runs == 0 && t.events.iter().any(|e| matches!(e, Event::Failed(_)))) || ticks > 20_000
    };
    if !over {
        return;
    }
    let events = world.resource::<Transcript>().events.clone();
    let log = rigcoder::effect_log(world);
    *world.resource::<Captured>().0.lock().unwrap() = Outcome_ { events, log: Some(log) };
    world.write_message(AppExit::Success);
}

fn call(name: &str, args: serde_json::Value) -> AssistantContent {
    AssistantContent::tool_call(format!("call-{name}-{args}"), name, args)
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rigcoder-replay-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build an app: a scripted model when `script` is given, the plugin in
/// `mode`, an optional checkpoint dir, and either a prompt to submit or a
/// scene to resume.
fn run(workspace: &std::path::Path, mode: Mode, prompt_override: Option<String>, script: Option<Vec<Vec<AssistantContent>>>, checkpoint: Option<std::path::PathBuf>, start: Start) -> Outcome_ {
    let captured = Arc::new(Mutex::new(Outcome_::default()));
    let mut app = App::new();
    app.add_plugins((
        ScheduleRunnerPlugin::run_loop(std::time::Duration::from_millis(1)),
        RigcoderPlugin { workspace: workspace.to_path_buf(), model: ModelChoice::parse("gemini", None).unwrap(), max_turns: 8, mode, prompt_override },
    ))
    .insert_resource(Checkpoint { dir: checkpoint, tar: false, turns_saved: 0 })
    .insert_resource(Captured(captured.clone()))
    .add_systems(bevy_app::Last, capture_when_over);
    if let Some(script) = script {
        app.insert_resource(ScriptedModel(Mutex::new(Some(Scripted(Mutex::new(script.into())))))).add_systems(PreStartup, register_scripted);
    }
    match start {
        Start::Prompt(prompt) => {
            app.add_systems(PostStartup, move |world: &mut World| {
                rigcoder::submit(world, prompt);
            });
        }
        Start::Resume(scene) => {
            app.add_systems(PostStartup, move |world: &mut World| {
                rigcoder::checkpoint::resume(world, &scene).expect("the scene loads");
            });
        }
    }
    app.run();
    let out = captured.lock().unwrap().clone();
    out
}

enum Start {
    Prompt(&'static str),
    Resume(WorldScene),
}

fn script() -> Vec<Vec<AssistantContent>> {
    vec![
        vec![call("write_file", serde_json::json!({"path": "hello.txt", "content": "hi\n"}))],
        vec![call("bash", serde_json::json!({"command": "cat hello.txt"}))],
        vec![AssistantContent::text("Wrote hello.txt and read it back: hi.")],
    ]
}

#[test]
fn a_recorded_run_replays_without_a_model_and_diverges_when_the_prompt_changes() {
    let dir = scratch("record");
    let live = run(&dir, Mode::Live, None, Some(script()), None, Start::Prompt("write hello.txt then cat it"));
    let log = live.log.expect("a log");
    assert!(live.events.iter().any(|e| matches!(e, Event::Settled { answer } if answer.contains("hi"))), "{:?}", live.events);
    assert!(log.records.len() >= 5, "3 completions and 2 tool calls: {}", log.records.len());

    // Same prompt and workspace (the preamble names the directory), no
    // model, no tools: the replayers answer from the log, and the file the
    // live run wrote is not written again.
    std::fs::remove_file(dir.join("hello.txt")).unwrap();
    let replayed = run(&dir, Mode::Replay(log.clone()), None, None, None, Start::Prompt("write hello.txt then cat it"));
    assert!(replayed.events.iter().any(|e| matches!(e, Event::Settled { answer } if answer.contains("hi"))), "{:?}", replayed.events);
    assert!(!dir.join("hello.txt").exists(), "a replayed tool call does not touch the disk");

    // A different system prompt makes the first request differ from its
    // record: the run fails with the divergence, no model is called.
    let diverged = run(&dir, Mode::Replay(log), Some("You are terse.".to_owned()), None, None, Start::Prompt("write hello.txt then cat it"));
    let failed = diverged.events.iter().find_map(|e| match e { Event::Failed(reason) => Some(reason.clone()), _ => None });
    assert!(failed.is_some(), "{:?}", diverged.events);
    assert!(!diverged.events.iter().any(|e| matches!(e, Event::Settled { .. })));
}

#[test]
fn a_checkpoint_between_turns_resumes_in_a_fresh_world() {
    let dir = scratch("checkpoint");
    let scenes = dir.join("scenes");
    let live = run(&dir, Mode::Live, None, Some(script()), Some(scenes.clone()), Start::Prompt("write hello.txt then cat it"));
    assert!(live.events.iter().any(|e| matches!(e, Event::Settled { .. })), "{:?}", live.events);
    let first = rigcoder::checkpoint::scene_path(&scenes, 1);
    assert!(first.is_file(), "a scene per turn: {}", first.display());
    let scene: WorldScene = serde_json::from_str(&std::fs::read_to_string(&first).unwrap()).unwrap();

    // A fresh world, the workspace as it was after turn 1 (hello.txt exists),
    // and a model that continues the script from turn 2.
    let resumed = run(&dir, Mode::Live, None, Some(script()[1..].to_vec()), None, Start::Resume(scene));
    assert!(resumed.events.iter().any(|e| matches!(e, Event::Settled { answer } if answer.contains("hi"))), "{:?}", resumed.events);
    assert!(resumed.events.iter().any(|e| matches!(e, Event::ToolCall { name, .. } if name == "bash")), "turn 2 ran: {:?}", resumed.events);
    assert!(!resumed.events.iter().any(|e| matches!(e, Event::ToolCall { name, .. } if name == "write_file")), "turn 1 did not rerun: {:?}", resumed.events);
}
