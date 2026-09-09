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
    serve::{Dispatch, Reply, Serve},
};
use rig_ecs::{agent::scene::WorldScene, bus::Handlers};
use rigcoder::{
    EffectLog, Event, Mode, ModelChoice, RigcoderPlugin, Transcript, checkpoint::Checkpoint,
};

struct Scripted(Mutex<std::collections::VecDeque<Vec<AssistantContent>>>);

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
    async fn serve(&self, _kind: EffectKind, _dispatch: Dispatch) -> Reply {
        let next = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![AssistantContent::text("(script over)")]);
        Reply::Outcome(Ok(Outcome::Completion(CompletionResponse::new(
            next,
            Usage::new(),
            "scripted",
        ))))
    }
}

#[derive(Resource)]
struct ScriptedModel(Mutex<Option<Scripted>>);

fn register_scripted(mut handlers: Handlers, model: Res<ScriptedModel>) {
    if let Some(model) = model.0.lock().unwrap().take() {
        handlers
            .register(rigcoder::model::MODEL_KEY, model)
            .expect("a fresh key");
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
        (c.runs > 0 && c.active.is_none())
            || (c.runs == 0 && t.events.iter().any(|e| matches!(e, Event::Failed { .. })))
            || ticks > 20_000
    };
    if !over {
        return;
    }
    let events = world.resource::<Transcript>().events.clone();
    let log = rigcoder::effect_log(world);
    *world.resource::<Captured>().0.lock().unwrap() = Outcome_ {
        events,
        log: Some(log),
    };
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
fn run(
    workspace: &std::path::Path,
    mode: Mode,
    prompt_override: Option<String>,
    script: Option<Vec<Vec<AssistantContent>>>,
    checkpoint: Option<std::path::PathBuf>,
    start: Start,
) -> Outcome_ {
    let captured = Arc::new(Mutex::new(Outcome_::default()));
    let mut app = App::new();
    app.add_plugins((
        ScheduleRunnerPlugin::run_loop(std::time::Duration::from_millis(1)),
        RigcoderPlugin {
            workspace: workspace.to_path_buf(),
            model: ModelChoice::parse("gemini", None).unwrap(),
            max_turns: 8,
            mode,
            prompt_override,
            keep_stream_events: false,
        },
    ))
    .insert_resource(Checkpoint {
        dir: checkpoint,
        tar: false,
        turns_saved: 0,
    })
    .insert_resource(Captured(captured.clone()))
    .add_systems(bevy_app::Last, capture_when_over);
    if let Some(script) = script {
        app.insert_resource(ScriptedModel(Mutex::new(Some(Scripted(Mutex::new(
            script.into(),
        ))))))
        .add_systems(PreStartup, register_scripted);
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
    captured.lock().unwrap().clone()
}

enum Start {
    Prompt(&'static str),
    Resume(WorldScene),
}

fn script() -> Vec<Vec<AssistantContent>> {
    vec![
        vec![call(
            "write_file",
            serde_json::json!({"path": "hello.txt", "content": "hi\n"}),
        )],
        vec![call(
            "bash",
            serde_json::json!({"command": "cat hello.txt"}),
        )],
        vec![AssistantContent::text(
            "Wrote hello.txt and read it back: hi.",
        )],
    ]
}

#[test]
fn a_recorded_run_replays_without_a_model_and_diverges_when_the_prompt_changes() {
    let dir = scratch("record");
    let live = run(
        &dir,
        Mode::Live,
        None,
        Some(script()),
        None,
        Start::Prompt("write hello.txt then cat it"),
    );
    let log = live.log.expect("a log");
    assert!(
        live.events
            .iter()
            .any(|e| matches!(e, Event::Settled { answer } if answer.contains("hi"))),
        "{:?}",
        live.events
    );
    assert!(
        log.records.len() >= 5,
        "3 completions and 2 tool calls: {}",
        log.records.len()
    );

    // Same prompt and workspace (the preamble names the directory), no
    // model, no tools: the replayers answer from the log, and the file the
    // live run wrote is not written again.
    std::fs::remove_file(dir.join("hello.txt")).unwrap();
    let replayed = run(
        &dir,
        Mode::Replay(log.clone().into()),
        None,
        None,
        None,
        Start::Prompt("write hello.txt then cat it"),
    );
    assert!(
        replayed
            .events
            .iter()
            .any(|e| matches!(e, Event::Settled { answer } if answer.contains("hi"))),
        "{:?}",
        replayed.events
    );
    assert!(
        !dir.join("hello.txt").exists(),
        "a replayed tool call does not touch the disk"
    );

    // A different system prompt makes the first request differ from its
    // record: the run fails with the divergence, no model is called.
    let diverged = run(
        &dir,
        Mode::Replay(log.into()),
        Some("You are terse.".to_owned()),
        None,
        None,
        Start::Prompt("write hello.txt then cat it"),
    );
    let failed = diverged.events.iter().find_map(|e| match e {
        Event::Failed { reason } => Some(reason.clone()),
        _ => None,
    });
    assert!(failed.is_some(), "{:?}", diverged.events);
    assert!(
        !diverged
            .events
            .iter()
            .any(|e| matches!(e, Event::Settled { .. }))
    );
}

#[test]
fn a_checkpoint_between_turns_resumes_in_a_fresh_world() {
    let dir = scratch("checkpoint");
    let scenes = dir.join("scenes");
    let live = run(
        &dir,
        Mode::Live,
        None,
        Some(script()),
        Some(scenes.clone()),
        Start::Prompt("write hello.txt then cat it"),
    );
    assert!(
        live.events
            .iter()
            .any(|e| matches!(e, Event::Settled { .. })),
        "{:?}",
        live.events
    );
    let first = rigcoder::checkpoint::scene_path(&scenes, 1);
    assert!(first.is_file(), "a scene per turn: {}", first.display());
    let scene: WorldScene =
        serde_json::from_str(&std::fs::read_to_string(&first).unwrap()).unwrap();

    // A fresh world, the workspace as it was after turn 1 (hello.txt exists),
    // and a model that continues the script from turn 2.
    let resumed = run(
        &dir,
        Mode::Live,
        None,
        Some(script()[1..].to_vec()),
        None,
        Start::Resume(scene),
    );
    assert!(
        resumed
            .events
            .iter()
            .any(|e| matches!(e, Event::Settled { answer } if answer.contains("hi"))),
        "{:?}",
        resumed.events
    );
    assert!(
        resumed
            .events
            .iter()
            .any(|e| matches!(e, Event::ToolCall { name, .. } if name == "bash")),
        "turn 2 ran: {:?}",
        resumed.events
    );
    assert!(
        !resumed
            .events
            .iter()
            .any(|e| matches!(e, Event::ToolCall { name, .. } if name == "write_file")),
        "turn 1 did not rerun: {:?}",
        resumed.events
    );
}

fn live_app(dir: &std::path::Path, script: Vec<Vec<AssistantContent>>) -> App {
    let mut app = App::new();
    app.add_plugins(RigcoderPlugin::live(
        dir.to_path_buf(),
        ModelChoice::parse("gemini", None).unwrap(),
        8,
    ))
    .insert_resource(ScriptedModel(Mutex::new(Some(Scripted(Mutex::new(
        script.into(),
    ))))))
    .add_systems(PreStartup, register_scripted);
    app.update();
    app
}

fn finish_app(app: &mut App) {
    for _ in 0..2_000 {
        app.update();
        if app
            .world()
            .resource::<rigcoder::Conversation>()
            .active
            .is_none()
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!(
        "run did not finish: {:?}",
        app.world().resource::<Transcript>().events
    );
}

#[test]
fn checkpoint_publication_scrubs_failure_diagnostics_and_can_resume() {
    use rig_ecs::agent::{Failed, Failure, Run, Settled};
    let dir = scratch("scrubbed-failure");
    let scenes = dir.join("scenes");
    let mut app = live_app(&dir, vec![vec![AssistantContent::text("done")]]);
    rigcoder::submit(app.world_mut(), "finish").unwrap();
    finish_app(&mut app);
    let run = app
        .world_mut()
        .query_filtered::<Entity, With<Run>>()
        .single(app.world())
        .unwrap();
    let message = "api_key=synthetic-checkpoint-secret";
    app.world_mut()
        .entity_mut(run)
        .remove::<Settled>()
        .insert(Failed(Failure::Provider(rig::error::ErrorReport::new(
            rig::error::ErrorKind::Request,
            message,
        ))));
    app.world_mut().insert_resource(Checkpoint {
        dir: Some(scenes.clone()),
        ..Default::default()
    });
    app.update();
    let bytes = std::fs::read(rigcoder::checkpoint::scene_path(&scenes, 1)).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-checkpoint-secret"));
    assert!(matches!(&app.world().get::<Failed>(run).unwrap().0,
        Failure::Provider(error) if error.message == message));
    let scene: WorldScene = serde_json::from_slice(&bytes).unwrap();
    let mut resumed = live_app(&dir, vec![]);
    rigcoder::checkpoint::resume(resumed.world_mut(), &scene).unwrap();
    finish_app(&mut resumed);
    assert!(rigcoder::effect_log(resumed.world()).records.is_empty());
    assert!(resumed.world().resource::<Transcript>().events.iter().any(
        |event| matches!(event, Event::Failed { reason } if reason.kind == "request"
            && reason.message == "[redacted]" && reason.retryable == Some(false))
    ));
}

#[test]
fn a_terminal_checkpoint_reports_its_saved_answer_without_another_model_call() {
    let dir = scratch("terminal");
    let scenes = dir.join("scenes");
    let mut app = live_app(&dir, vec![vec![AssistantContent::text("already done")]]);
    app.world_mut().insert_resource(Checkpoint {
        dir: Some(scenes.clone()),
        ..Default::default()
    });
    rigcoder::submit(app.world_mut(), "finish").unwrap();
    finish_app(&mut app);
    let scene: WorldScene = serde_json::from_slice(
        &std::fs::read(rigcoder::checkpoint::scene_path(&scenes, 1)).unwrap(),
    )
    .unwrap();
    let mut resumed = live_app(&dir, vec![]);
    rigcoder::checkpoint::resume(resumed.world_mut(), &scene).unwrap();
    assert!(
        resumed
            .world()
            .resource::<rigcoder::Conversation>()
            .active
            .is_none()
    );
    assert!(
        resumed
            .world()
            .resource::<Transcript>()
            .events
            .iter()
            .any(|e| matches!(e, Event::Settled { answer } if answer == "already done"))
    );
    assert_eq!(
        resumed
            .world()
            .resource::<rigcoder::checkpoint::MaterialisedTurns>()
            .0,
        1
    );
    assert_eq!(resumed.world().resource::<Checkpoint>().turns_saved, 1);
    assert!(rigcoder::effect_log(resumed.world()).records.is_empty());
}

#[test]
fn resuming_a_later_run_skips_old_terminal_runs_and_continues_checkpoint_numbers() {
    let dir = scratch("later-run");
    let scenes = dir.join("scenes");
    let mut app = live_app(
        &dir,
        vec![
            vec![AssistantContent::text("first run")],
            vec![call(
                "write_file",
                serde_json::json!({"path": "second.txt", "content": "once"}),
            )],
            vec![AssistantContent::text("second run")],
        ],
    );
    app.world_mut().insert_resource(Checkpoint {
        dir: Some(scenes.clone()),
        ..Default::default()
    });
    rigcoder::submit(app.world_mut(), "first").unwrap();
    finish_app(&mut app);
    rigcoder::submit(app.world_mut(), "second").unwrap();
    finish_app(&mut app);
    let scene: WorldScene = serde_json::from_slice(
        &std::fs::read(rigcoder::checkpoint::scene_path(&scenes, 2)).unwrap(),
    )
    .unwrap();
    let restored_scenes = dir.join("restored-scenes");
    let mut resumed = live_app(&dir, vec![vec![AssistantContent::text("continued second")]]);
    resumed.world_mut().insert_resource(Checkpoint {
        dir: Some(restored_scenes.clone()),
        ..Default::default()
    });
    rigcoder::checkpoint::resume(resumed.world_mut(), &scene).unwrap();
    assert_eq!(resumed.world().resource::<Checkpoint>().turns_saved, 2);
    finish_app(&mut resumed);
    assert!(
        resumed
            .world()
            .resource::<Transcript>()
            .events
            .iter()
            .any(|e| matches!(e, Event::Settled { answer } if answer == "continued second"))
    );
    assert!(!rigcoder::checkpoint::scene_path(&restored_scenes, 1).exists());
    assert!(rigcoder::checkpoint::scene_path(&restored_scenes, 3).is_file());
}

#[test]
fn checkpoint_archives_require_a_directory_outside_the_workspace() {
    let dir = scratch("archive-recursion");
    let scenes = dir.join("scenes");
    let mut app = live_app(&dir, vec![vec![AssistantContent::text("done")]]);
    app.world_mut().insert_resource(Checkpoint {
        dir: Some(scenes.clone()),
        tar: true,
        turns_saved: 0,
    });
    rigcoder::submit(app.world_mut(), "finish").unwrap();
    finish_app(&mut app);
    assert!(app.world().resource::<Transcript>().events.iter().any(
        |e| matches!(e, Event::Failed { reason: error } if error.kind == "checkpoint" && error.message.contains("outside the workspace"))
    ));
    assert!(!rigcoder::checkpoint::scene_path(&scenes, 1).exists());
    assert!(!rigcoder::checkpoint::tar_path(&scenes, 1).exists());
    assert_eq!(app.world().resource::<Checkpoint>().turns_saved, 0);
    use rig::observe::HostAction as _;
    let failures: Vec<_> = rigcoder::observations(app.world())
        .unwrap()
        .observations
        .into_iter()
        .filter_map(|observation| {
            rigcoder::failure::FailureDetail::from_action(&observation.action)
        })
        .map(Result::unwrap)
        .filter(|failure| failure.kind == "checkpoint")
        .collect();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].adapter.is_none());
}

#[test]
fn a_checkpoint_archive_and_scene_are_published_together() {
    let dir = scratch("archive");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("kept.txt"), "checkpoint contents").unwrap();
    let scenes = dir.join("scenes");
    let mut app = live_app(&workspace, vec![vec![AssistantContent::text("done")]]);
    app.world_mut().insert_resource(Checkpoint {
        dir: Some(scenes.clone()),
        tar: true,
        turns_saved: 0,
    });
    rigcoder::submit(app.world_mut(), "finish").unwrap();
    finish_app(&mut app);
    assert!(rigcoder::checkpoint::scene_path(&scenes, 1).is_file());
    let output = std::process::Command::new("tar")
        .arg("-xOf")
        .arg(rigcoder::checkpoint::tar_path(&scenes, 1))
        .arg("./kept.txt")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"checkpoint contents");
}

#[test]
fn replay_rejects_changed_run_settings_before_dispatch() {
    let dir = scratch("settings");
    let live = run(
        &dir,
        Mode::Live,
        None,
        Some(vec![vec![AssistantContent::text("done")]]),
        None,
        Start::Prompt("finish"),
    );
    let mut app = App::new();
    app.add_plugins(RigcoderPlugin {
        workspace: dir,
        model: ModelChoice::parse("gemini", None).unwrap(),
        max_turns: 7,
        mode: Mode::Replay(live.log.unwrap().into()),
        prompt_override: None,
        keep_stream_events: false,
    });
    app.update();
    rigcoder::submit(app.world_mut(), "finish").unwrap();
    assert!(
        app.world()
            .resource::<Transcript>()
            .events
            .iter()
            .any(|e| matches!(e, Event::Failed { reason: error } if error.kind == "internal" && error.origin == "replay_validation" && error.is_replay_failure() && error.adapter.is_none())),
        "{:?}", app.world().resource::<Transcript>().events
    );
    assert!(
        rigcoder::effect_log(app.world()).records.is_empty(),
        "no effects dispatched"
    );
}

#[test]
fn a_failed_tar_command_does_not_publish_a_scene() {
    let dir = scratch("failed-tar");
    let workspace = dir.join("not-a-directory");
    std::fs::write(&workspace, "not a workspace").unwrap();
    let scenes = dir.join("scenes");
    let mut app = live_app(&workspace, vec![vec![AssistantContent::text("done")]]);
    app.world_mut().insert_resource(Checkpoint {
        dir: Some(scenes.clone()),
        tar: true,
        turns_saved: 0,
    });
    rigcoder::submit(app.world_mut(), "finish").unwrap();
    finish_app(&mut app);
    assert!(
        app.world()
            .resource::<Transcript>()
            .events
            .iter()
            .any(|e| matches!(e, Event::Failed { reason: error } if error.kind == "checkpoint" && error.message.contains("tar failed")))
    );
    assert!(!rigcoder::checkpoint::scene_path(&scenes, 1).exists());
    assert!(!rigcoder::checkpoint::tar_path(&scenes, 1).exists());
    assert!(!scenes.join("turn-001.tar.partial").exists());
}

#[test]
fn the_effect_log_retains_handler_output_before_history_shaping() {
    let dir = scratch("full-handler-output");
    let content = format!("{}MIDDLE-MARKER{}", "h".repeat(20_000), "t".repeat(20_000));
    std::fs::write(dir.join("large.txt"), &content).unwrap();
    let mut app = live_app(
        &dir,
        vec![
            vec![call("read_file", serde_json::json!({"path": "large.txt"}))],
            vec![AssistantContent::text("done")],
        ],
    );
    app.world_mut()
        .resource_mut::<rigcoder::steer::Steer>()
        .max_result_chars = 200;
    rigcoder::submit(app.world_mut(), "read the file").unwrap();
    finish_app(&mut app);
    let log = rigcoder::effect_log(app.world());
    // The recorded tool answer retains its middle, including text beyond
    // the previous unconditional 30,000-character wrapper cap.
    let wire = serde_json::to_string(&log).unwrap();
    assert!(wire.contains("MIDDLE-MARKER"));
    let requests: Vec<_> = log
        .iter()
        .filter_map(|record| match &record.kind {
            EffectKind::Completion { request, .. } => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 2);
    let second = serde_json::to_string(&requests[1]).unwrap();
    assert!(second.contains("result cut to 200 chars"));
    assert!(!second.contains("MIDDLE-MARKER"));
}

#[test]
fn checkpoint_collisions_preserve_the_existing_snapshot() {
    let dir = scratch("checkpoint-collision");
    let scenes = dir.join("scenes");
    std::fs::create_dir_all(&scenes).unwrap();
    let existing = rigcoder::checkpoint::scene_path(&scenes, 1);
    std::fs::write(&existing, "existing snapshot").unwrap();
    let mut app = live_app(&dir, vec![vec![AssistantContent::text("done")]]);
    app.world_mut().insert_resource(Checkpoint {
        dir: Some(scenes),
        ..Default::default()
    });
    rigcoder::submit(app.world_mut(), "finish").unwrap();
    finish_app(&mut app);
    assert_eq!(
        std::fs::read_to_string(existing).unwrap(),
        "existing snapshot"
    );
    assert!(
        app.world().resource::<Transcript>().events.iter().any(
            |e| matches!(e, Event::Failed { reason: error } if error.kind == "checkpoint" && error.message.contains("already exists"))
        )
    );
}
