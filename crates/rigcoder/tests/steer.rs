//! The steering systems, pinned the way rig-ecs pins its hook rows: a
//! scripted model, a run, and the transcript and requests that result.

use std::sync::{Arc, Mutex};

use bevy_app::{App, AppExit, PostStartup, PreStartup, ScheduleRunnerPlugin};
use bevy_ecs::prelude::*;
use rig::{
    completion::{CompletionResponse, ModelRef, ProviderCapabilities, Usage},
    effect::{EffectKind, FamilyDescriptor, HandlerDescriptor, HandlerKey, Outcome},
    message::AssistantContent,
    serve::{Dispatch, Reply, Serve},
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
            family: FamilyDescriptor::Completion {
                model: ModelRef::new("scripted"),
                capabilities: ProviderCapabilities::default(),
            },
            layers: Vec::new(),
        }
    }

    async fn serve(&self, kind: EffectKind, _dispatch: Dispatch) -> Reply {
        if let EffectKind::Completion { request, .. } = &kind {
            self.seen.lock().unwrap().push(
                request
                    .chat_history
                    .iter()
                    .filter_map(MessageParts::from_message)
                    .collect(),
            );
        }
        let next = self
            .turns
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

/// The transcript, copied out once the run is over: the world is not
/// readable after `App::run` returns under the schedule runner.
#[derive(Resource)]
struct Captured(Arc<Mutex<Vec<Event>>>);

fn capture_when_over(
    conversation: Res<rigcoder::Conversation>,
    transcript: Res<Transcript>,
    captured: Res<Captured>,
    mut ticks: Local<usize>,
    mut exit: MessageWriter<AppExit>,
) {
    *ticks += 1;
    let failed_setup = conversation.runs == 0
        && transcript
            .events
            .iter()
            .any(|e| matches!(e, Event::Failed(_)));
    if (conversation.runs > 0 && conversation.active.is_none()) || failed_setup || *ticks > 20_000 {
        *captured.0.lock().unwrap() = transcript.events.clone();
        exit.write(AppExit::Success);
    }
}

fn register_scripted(mut handlers: Handlers, model: Res<ScriptedModel>) {
    if let Some(model) = model.0.lock().unwrap().take() {
        handlers
            .register(rigcoder::model::MODEL_KEY, model)
            .expect("a fresh key");
    }
}

fn call(name: &str, args: serde_json::Value) -> AssistantContent {
    AssistantContent::tool_call(format!("call-{name}-{args}"), name, args)
}

/// Run `prompt` against a scripted model with the given steering rules;
/// returns the transcript and every request the model saw.
fn run_scripted(
    workspace: &std::path::Path,
    steer: Steer,
    script: Vec<Vec<AssistantContent>>,
    prompt: &'static str,
) -> (Vec<Event>, Vec<Vec<MessageParts>>) {
    run_scripted_with_scope(workspace, steer, Default::default(), script, prompt)
}

fn run_scripted_with_scope(
    workspace: &std::path::Path,
    steer: Steer,
    scope: rigcoder::steer::Scope,
    script: Vec<Vec<AssistantContent>>,
    prompt: &'static str,
) -> (Vec<Event>, Vec<Vec<MessageParts>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let model = Scripted {
        turns: Mutex::new(script.into()),
        seen: seen.clone(),
    };
    let mut app = App::new();
    app.add_plugins((
        ScheduleRunnerPlugin::run_loop(std::time::Duration::from_millis(1)),
        RigcoderPlugin::live(
            workspace.to_path_buf(),
            ModelChoice::parse("gemini", None).unwrap(),
            8,
        ),
    ))
    .insert_resource(steer)
    .insert_resource(scope)
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
        vec![
            vec![call(
                "bash",
                serde_json::json!({"command": "find / -name report.jsonl"}),
            )],
            vec![AssistantContent::text("ok")],
        ],
        "look around",
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Denied { name, .. } if name == "bash")),
        "{events:?}"
    );
    // The denial is what the model reads as the tool's result; nothing ran.
    assert!(events.iter().any(|e| matches!(e, Event::ToolResult { ok: false, output, .. } if output.starts_with("denied:"))), "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ToolResult { ok: true, .. })),
        "the tool never ran: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Assistant { .. }))
            .count(),
        1,
        "the answer is shown once: {events:?}"
    );
    let second = format!("{:?}", seen[1]);
    assert!(
        second.contains("denied") && second.contains("workspace"),
        "{second}"
    );
}

#[test]
fn an_over_long_result_is_cut_for_history_and_the_run_goes_on() {
    let dir = scratch("shape");
    let steer = Steer {
        max_result_chars: 200,
        ..Default::default()
    };
    let (events, seen) = run_scripted(
        &dir,
        steer,
        vec![
            vec![call(
                "bash",
                serde_json::json!({"command": "yes line | head -n 300"}),
            )],
            vec![AssistantContent::text("done")],
        ],
        "print a lot",
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Settled { .. })),
        "{events:?}"
    );
    let second = format!("{:?}", seen[1]);
    assert!(second.contains("result cut to 200 chars"), "{second}");
}

#[test]
fn a_text_only_answer_with_a_deliverable_missing_is_retried_then_accepted() {
    let dir = scratch("deliverable");
    let report = dir.join("report.jsonl");
    let steer = Steer {
        deliverables: vec![report.clone()],
        ..Default::default()
    };
    let (events, seen) = run_scripted(
        &dir,
        steer,
        vec![
            vec![AssistantContent::text(
                "I think the answer is cwe-93. Should I write it?",
            )],
            vec![call(
                "write_file",
                serde_json::json!({"path": report.display().to_string(), "content": "{\"cwe_id\": [\"cwe-93\"]}\n"}),
            )],
            vec![AssistantContent::text("Wrote the report.")],
        ],
        "write the report",
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Settled { answer } if answer == "Wrote the report.")),
        "{events:?}"
    );
    assert!(report.is_file());
    let second = format!("{:?}", seen[1]);
    assert!(
        second.contains("Not finished") && second.contains("report.jsonl"),
        "{second}"
    );
    assert_eq!(seen.len(), 3);
}

#[test]
fn invalid_steering_regexes_fail_closed() {
    let dir = scratch("invalid-regex");
    let steer = Steer {
        hold: vec!["(".to_owned()],
        ..Default::default()
    };
    let (events, _) = run_scripted(
        &dir,
        steer,
        vec![
            vec![call(
                "bash",
                serde_json::json!({"command": "touch forbidden"}),
            )],
            vec![AssistantContent::text("done")],
        ],
        "write a file",
    );
    assert!(!dir.join("forbidden").exists());
    assert!(events.iter().any(
        |e| matches!(e, Event::Denied { reason, .. } if reason.contains("invalid steering rule"))
    ));
}

#[test]
fn held_commands_wait_for_a_decision_and_only_approved_calls_run() {
    for approve in [true, false] {
        let dir = scratch(if approve { "approve" } else { "reject" });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new();
        app.add_plugins(RigcoderPlugin::live(
            dir.clone(),
            ModelChoice::parse("gemini", None).unwrap(),
            8,
        ))
        .insert_resource(Steer {
            hold: vec!["^touch".to_owned()],
            auto_approve: false,
            ..Default::default()
        })
        .insert_resource(ScriptedModel(Mutex::new(Some(Scripted {
            turns: Mutex::new(
                vec![
                    vec![call(
                        "bash",
                        serde_json::json!({"command": "touch decided"}),
                    )],
                    vec![AssistantContent::text("done")],
                ]
                .into(),
            ),
            seen,
        }))))
        .add_systems(PreStartup, register_scripted);
        app.update();
        rigcoder::submit(app.world_mut(), "write a file").unwrap();
        let mut decided = false;
        for _ in 0..2_000 {
            app.update();
            if !decided
                && !app
                    .world()
                    .resource::<rigcoder::steer::Approvals>()
                    .pending
                    .is_empty()
            {
                assert!(!dir.join("decided").exists(), "held command did not run");
                let mut approvals = app.world_mut().resource_mut::<rigcoder::steer::Approvals>();
                if approve {
                    approvals.approve_next()
                } else {
                    approvals.deny_next()
                }
                decided = true;
            }
            if app
                .world()
                .resource::<rigcoder::Conversation>()
                .active
                .is_none()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(decided);
        assert!(
            app.world()
                .resource::<rigcoder::Conversation>()
                .active
                .is_none()
        );
        assert_eq!(dir.join("decided").exists(), approve);
    }
}

#[test]
fn scope_blocks_bash_traversal_and_symlinks_but_allows_the_exact_note() {
    let dir = scratch("scope").canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("allowed")).unwrap();
    std::fs::create_dir_all(dir.join("harness/notes")).unwrap();
    std::fs::write(dir.join("harness/ledger.jsonl"), "untouched").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("harness"), dir.join("allowed/linked")).unwrap();
    let scope = rigcoder::steer::Scope {
        root: dir.clone(),
        allow: vec![dir.join("allowed"), dir.join("harness/notes/current.md")],
        deny: vec![dir.join("harness")],
    };
    let mut calls = vec![
        call("bash", serde_json::json!({"command": "rm -rf harness"})),
        call(
            "bash",
            serde_json::json!({"command": "python3 -c \"open('harness/ledger.jsonl', 'w').write('hacked')\""}),
        ),
        call(
            "write_file",
            serde_json::json!({"path": "allowed/../harness/ledger.jsonl", "content": "hacked"}),
        ),
        call(
            "write_file",
            serde_json::json!({"path": "harness/notes/other.md", "content": "hacked"}),
        ),
        call(
            "write_file",
            serde_json::json!({"path": "harness/notes/current.md", "content": "approved note"}),
        ),
        call(
            "write_file",
            serde_json::json!({"path": "stray/../allowed/ok.txt", "content": "approved write"}),
        ),
    ];
    #[cfg(unix)]
    calls.push(call(
        "write_file",
        serde_json::json!({"path": "allowed/linked/ledger.jsonl", "content": "hacked"}),
    ));
    let (events, _) = run_scripted_with_scope(
        &dir,
        Steer::default(),
        scope,
        vec![calls, vec![AssistantContent::text("done")]],
        "make scoped changes",
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("harness/ledger.jsonl")).unwrap(),
        "untouched"
    );
    assert!(!dir.join("harness/notes/other.md").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("harness/notes/current.md")).unwrap(),
        "approved note"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("allowed/ok.txt")).unwrap(),
        "approved write"
    );
    assert!(
        !dir.join("stray").exists(),
        "canonical arguments avoid creating unscoped parents"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Denied { name, .. } if name == "bash"))
    );
}

#[cfg(unix)]
#[test]
fn scope_resolves_symlinks_before_parent_components_and_rejects_hard_links() {
    let dir = scratch("scope-path-rules").canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("allowed/sub")).unwrap();
    std::fs::create_dir_all(dir.join("denied/sub")).unwrap();
    std::fs::write(dir.join("denied/secret"), "untouched").unwrap();
    std::os::unix::fs::symlink(dir.join("denied/sub"), dir.join("allowed/link")).unwrap();
    std::os::unix::fs::symlink(dir.join("missing"), dir.join("allowed/dangling")).unwrap();
    std::fs::hard_link(dir.join("denied/secret"), dir.join("allowed/hardlink")).unwrap();
    let scope = rigcoder::steer::Scope {
        root: dir.clone(),
        allow: vec![dir.join("allowed")],
        deny: vec![dir.join("denied")],
    };
    for path in [
        "allowed/link/../secret",
        "allowed/dangling",
        "allowed/hardlink",
        "allowed/../denied/secret",
        "allowed/../",
    ] {
        assert!(
            scope.violation(std::iter::once(path)).is_some(),
            "must deny {path}"
        );
    }
    assert!(
        scope
            .violation(std::iter::once("allowed/sub/new.txt"))
            .is_none()
    );
}

fn approval_app(
    dir: &std::path::Path,
    mode: rigcoder::approval::ApprovalMode,
    calls: Vec<AssistantContent>,
) -> App {
    let mut app = App::new();
    app.add_plugins(RigcoderPlugin::live(
        dir.to_path_buf(),
        ModelChoice::parse("gemini", None).unwrap(),
        8,
    ))
    .insert_resource(mode)
    .insert_resource(ScriptedModel(Mutex::new(Some(Scripted {
        turns: Mutex::new(vec![calls, vec![AssistantContent::text("done")]].into()),
        seen: Arc::new(Mutex::new(Vec::new())),
    }))))
    .add_systems(PreStartup, register_scripted);
    app.update();
    rigcoder::submit(app.world_mut(), "perform the requested operation").unwrap();
    app
}

fn advance_until(app: &mut App, condition: impl Fn(&World) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !condition(app.world()) {
        assert!(
            std::time::Instant::now() < deadline,
            "run did not reach the expected state: {:?}",
            app.world().resource::<Transcript>().events
        );
        app.update();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn rust_edit_approval_shows_formatted_bytes_and_repeated_gates_cannot_release_it() {
    use rigcoder::approval::{ApprovalMode, Approvals};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rs");
    let before = format!("fn main() {{{}let x = 1; }}\n", " ".repeat(1000));
    std::fs::write(&path, &before).unwrap();
    let mut app = approval_app(
        dir.path(),
        ApprovalMode::Ask,
        vec![call(
            "edit_file",
            serde_json::json!({"path":"main.rs", "old_string":"1", "new_string":"2"}),
        )],
    );
    advance_until(&mut app, |world| {
        !world.resource::<Approvals>().pending.is_empty()
    });
    let request = app.world().resource::<Approvals>().pending[0].clone();
    let preview = request.file.as_ref().unwrap();
    assert_eq!(preview.after, "fn main() {\n    let x = 2;\n}\n");
    assert_eq!(request.operation_id.len(), 64);
    assert!(preview.before_sha256.is_some());
    assert!(preview.diff.contains("let x = 2;"));
    for _ in 0..30 {
        app.update();
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(app.world().resource::<Approvals>().pending.len(), 1);
    assert!(
        app.world_mut()
            .resource_mut::<Approvals>()
            .decide(&request.operation_id, true)
    );
    advance_until(&mut app, |world| {
        !world.resource::<rigcoder::Conversation>().is_busy()
    });
    assert_eq!(std::fs::read_to_string(&path).unwrap(), preview.after);
    assert_eq!(app.world().resource::<Transcript>().events.iter().filter(|event| matches!(event, Event::ToolResult { name, ok: true, .. } if name == "edit_file")).count(), 1);
    assert!(
        !app.world_mut()
            .resource_mut::<Approvals>()
            .decide(&request.operation_id, true)
    );
}

#[test]
fn denied_cancelled_changed_arguments_and_stale_sources_cannot_use_an_old_approval() {
    use rigcoder::approval::{ApprovalMode, Approvals};
    for scenario in ["deny", "cancel", "arguments", "source"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "before").unwrap();
        let mut app = approval_app(
            dir.path(),
            ApprovalMode::Ask,
            vec![call(
                "write_file",
                serde_json::json!({"path":"file.txt", "content":"after"}),
            )],
        );
        advance_until(&mut app, |world| {
            !world.resource::<Approvals>().pending.is_empty()
        });
        let id = app.world().resource::<Approvals>().pending[0]
            .operation_id
            .clone();
        match scenario {
            "deny" => assert!(
                app.world_mut()
                    .resource_mut::<Approvals>()
                    .decide(&id, false)
            ),
            "cancel" => {
                rigcoder::cancel(app.world_mut(), "cancel at approval");
            }
            "arguments" => {
                let world = app.world_mut();
                let mut calls = world.query::<(
                    &rig_ecs::agent::ToolCallSlot,
                    &mut rig_ecs::bus::PendingEffect,
                )>();
                for (slot, mut effect) in calls.iter_mut(world) {
                    if slot.name == "write_file"
                        && let EffectKind::ToolCall { args, .. } = &mut effect.kind
                    {
                        *args = serde_json::json!({"path":"other.txt", "content":"unapproved"})
                            .to_string();
                    }
                }
                assert!(world.resource_mut::<Approvals>().decide(&id, true));
            }
            "source" => {
                std::fs::write(&path, "external").unwrap();
                assert!(
                    app.world_mut()
                        .resource_mut::<Approvals>()
                        .decide(&id, true)
                );
            }
            _ => unreachable!(),
        }
        advance_until(&mut app, |world| {
            !world.resource::<rigcoder::Conversation>().is_busy()
        });
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            if scenario == "source" {
                "external"
            } else {
                "before"
            },
            "{scenario}"
        );
        assert!(!dir.path().join("other.txt").exists());
        assert!(
            !app.world_mut()
                .resource_mut::<Approvals>()
                .decide(&id, true)
        );
    }
}

#[test]
fn approval_holds_preserve_tool_concurrency_and_use_distinct_operation_ids() {
    use rigcoder::approval::{ApprovalMode, Approvals};
    let dir = tempfile::tempdir().unwrap();
    let calls = (0..6)
        .map(|index| {
            call(
                "write_file",
                serde_json::json!({"path":format!("{index}.txt"), "content":index.to_string()}),
            )
        })
        .collect();
    let mut app = approval_app(dir.path(), ApprovalMode::Ask, calls);
    let agent = app.world().resource::<rigcoder::AgentHandle>().agent;
    app.world_mut()
        .entity_mut(agent)
        .insert(rig_ecs::agent::ToolPolicy { concurrency: 1 });
    let mut ids = std::collections::HashSet::new();
    for index in 0..6 {
        advance_until(&mut app, |world| {
            !world.resource::<Approvals>().pending.is_empty()
        });
        assert_eq!(app.world().resource::<Approvals>().pending.len(), 1);
        let request = app.world().resource::<Approvals>().pending[0].clone();
        assert!(ids.insert(request.operation_id.clone()));
        assert!(!dir.path().join(format!("{index}.txt")).exists());
        for later in index + 1..6 {
            assert!(!dir.path().join(format!("{later}.txt")).exists());
        }
        app.world_mut().resource_mut::<Approvals>().approve_next();
    }
    advance_until(&mut app, |world| {
        !world.resource::<rigcoder::Conversation>().is_busy()
    });
    for index in 0..6 {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(format!("{index}.txt"))).unwrap(),
            index.to_string()
        );
    }
}

#[test]
fn deny_mode_refuses_files_and_commands_without_asking() {
    use rigcoder::approval::{ApprovalMode, Approvals};
    let dir = tempfile::tempdir().unwrap();
    let mut app = approval_app(
        dir.path(),
        ApprovalMode::Deny,
        vec![
            call(
                "write_file",
                serde_json::json!({"path":"new/sub/file", "content":"after"}),
            ),
            call("bash", serde_json::json!({"command":"touch command-ran"})),
        ],
    );
    advance_until(&mut app, |world| {
        !world.resource::<rigcoder::Conversation>().is_busy()
    });
    assert!(app.world().resource::<Approvals>().pending.is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn cancelling_an_issued_bash_command_stops_its_later_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = approval_app(
        dir.path(),
        rigcoder::approval::ApprovalMode::Auto,
        vec![call(
            "bash",
            serde_json::json!({"command":"touch started; sleep 1; touch unapproved-late-write"}),
        )],
    );
    advance_until(&mut app, |_| dir.path().join("started").exists());
    rigcoder::cancel(app.world_mut(), "cancel running command");
    advance_until(&mut app, |world| {
        !world.resource::<rigcoder::Conversation>().is_busy()
    });
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(!dir.path().join("unapproved-late-write").exists());
}

#[test]
fn cancellation_between_approval_and_dispatch_prevents_the_write() {
    use rig_ecs::bus::{BusSet, RigSchedule, ToolInputs};
    use rigcoder::approval::{ApprovalMode, Approvals, PolicyHold};
    #[derive(Resource, Default)]
    struct CancelledBeforeDispatch(bool);
    fn cancel_approved(world: &mut World) {
        if world.resource::<CancelledBeforeDispatch>().0 {
            return;
        }
        let approved = world
            .query_filtered::<&rig_ecs::agent::ToolCallSlot, (
                With<ToolInputs>,
                Without<PolicyHold>,
                Without<rig_ecs::bus::Issued>,
            )>()
            .iter(world)
            .any(|slot| slot.name == "write_file");
        if approved {
            world.resource_mut::<CancelledBeforeDispatch>().0 = true;
            rigcoder::cancel(world, "cancel approved call before dispatch");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = approval_app(
        dir.path(),
        ApprovalMode::Ask,
        vec![call(
            "write_file",
            serde_json::json!({"path": "new.txt", "content": "after"}),
        )],
    );
    app.init_resource::<CancelledBeforeDispatch>().add_systems(
        RigSchedule,
        cancel_approved.after(BusSet::Gate).before(BusSet::Dispatch),
    );
    advance_until(&mut app, |world| {
        !world.resource::<Approvals>().pending.is_empty()
    });
    app.world_mut().resource_mut::<Approvals>().approve_next();
    advance_until(&mut app, |world| {
        !world.resource::<rigcoder::Conversation>().is_busy()
    });
    assert!(app.world().resource::<CancelledBeforeDispatch>().0);
    assert!(!dir.path().join("new.txt").exists());
}
