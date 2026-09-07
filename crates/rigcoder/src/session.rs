//! The conversation as it happens: what the bus writes, read into a
//! transcript the UI and the CLI render. History is the previous run's
//! utterances, read back off the graph when it settles.

use std::collections::HashMap;

use bevy_ecs::prelude::*;
use rig::{
    effect::{EffectKind, Outcome},
    message::{AssistantContent, UserContent},
};
use rig_ecs::{
    agent::{MessageParts, Order, Parts, RunResult, ToolCallSlot, Usage, Utterance},
    bus::PendingEffect,
    prelude::*,
    systems::spawn_run,
};
use serde::Serialize;

/// The entities `setup` made.
#[derive(Resource, Debug, Clone)]
pub struct AgentHandle {
    pub agent: Entity,
    pub model: Entity,
    pub tools: Vec<Entity>,
}

/// The conversation's state across runs.
#[derive(Resource, Debug, Default)]
pub struct Conversation {
    /// Every utterance of every settled run, in order: the next run's history.
    pub history: Vec<MessageParts>,
    /// The run in flight, if any.
    pub active: Option<Entity>,
    /// How much of each streaming effect's text has been shown.
    shown: HashMap<Entity, usize>,
    /// Runs started, ever.
    pub runs: usize,
    /// The last prompt and the history it was submitted after, so a run
    /// that fails on a transient provider error can be submitted again.
    last: Option<(String, Vec<MessageParts>, crate::RunSettings)>,
    /// A resubmission due at this instant.
    retry_at: Option<std::time::Instant>,
    pub provider_retries: usize,
}

/// Transient provider failures are retried this many times, with backoff.
pub const MAX_PROVIDER_RETRIES: usize = 3;

impl Conversation {
    pub fn has_pending_retry(&self) -> bool {
        self.retry_at.is_some()
    }

    /// A provider backoff still belongs to the current user request.
    pub fn is_busy(&self) -> bool {
        self.active.is_some() || self.has_pending_retry()
    }
}

/// One thing that happened, in the order it happened.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    User {
        text: String,
    },
    /// Assistant text; a streaming answer grows the last one of these.
    Assistant {
        text: String,
    },
    ToolCall {
        name: String,
        args: String,
    },
    ToolResult {
        name: String,
        output: String,
        ok: bool,
    },
    Settled {
        answer: String,
    },
    Failed(String),
    /// The run failed on a transient provider error and will be submitted again.
    Retrying {
        reason: String,
        attempt: usize,
        wait_secs: u64,
    },
    /// A tool call the gate refused; the model reads the reason as the result.
    Denied {
        name: String,
        reason: String,
    },
    /// A tool call waiting for approval.
    Held {
        name: String,
        args: String,
    },
    /// The run's token usage, summed over its completions; written once
    /// when the run ends, before `Settled` or `Failed`.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        total_tokens: u64,
    },
}

#[derive(Resource, Debug, Default)]
pub struct Transcript {
    pub events: Vec<Event>,
}

impl Transcript {
    pub fn push(&mut self, event: Event) {
        self.events.push(event);
    }

    /// Grow the last assistant event, or start one.
    pub fn append_text(&mut self, delta: &str) {
        if let Some(Event::Assistant { text }) = self.events.last_mut() {
            text.push_str(delta);
        } else {
            self.events.push(Event::Assistant {
                text: delta.to_owned(),
            });
        }
    }
}

/// Start a run over the agent with `prompt`, after the history so far.
/// Refused (returns `None`) while a run is in flight or the agent is missing.
pub fn submit(world: &mut World, prompt: &str) -> Option<Entity> {
    if world.resource::<Conversation>().is_busy() {
        return None;
    }
    world.resource_mut::<Conversation>().provider_retries = 0;
    let settings = world.resource::<crate::RunSettings>().clone();
    start_run(world, prompt, true, settings)
}

fn start_run(
    world: &mut World,
    prompt: &str,
    announce: bool,
    settings: crate::RunSettings,
) -> Option<Entity> {
    let agent = world.get_resource::<AgentHandle>()?.agent;
    if world.resource::<Conversation>().active.is_some() {
        return None;
    }
    if settings.max_tokens == 0 {
        world.resource_mut::<Transcript>().push(Event::Failed(
            "max_tokens must be greater than zero".to_owned(),
        ));
        return None;
    }
    let Some(sequence) = world.resource::<Conversation>().runs.checked_add(1) else {
        world
            .resource_mut::<Transcript>()
            .push(Event::Failed("run sequence exhausted".to_owned()));
        return None;
    };
    world
        .entity_mut(agent)
        .insert(rig_ecs::agent::MaxTokens(Some(settings.max_tokens)));
    let history = world.resource::<Conversation>().history.clone();
    // Recorded handlers retain their old descriptors. Include the current
    // implementation and steering configuration in the declared policy so
    // replay cannot silently approve a changed tool or custom system.
    let policy = rig_effect_log::stable_hash(&(
        env!("CARGO_PKG_VERSION"),
        include_str!("lib.rs"),
        include_str!("model.rs"),
        include_str!("checkpoint.rs"),
        include_str!("tools.rs"),
        include_str!("file_change.rs"),
        include_str!("steer.rs"),
        include_str!("session.rs"),
        &settings,
        world.resource::<crate::steer::Steer>(),
        world
            .get_resource::<crate::steer::Scope>()
            .filter(|scope| scope.enabled()),
    ))
    .expect("policy contains only serializable strings and settings");
    world
        .entity_mut(agent)
        .insert(rig_ecs::agent::PolicyVersion(format!(
            "rigcoder-{policy:016x}"
        )));
    if announce {
        world.resource_mut::<Transcript>().push(Event::User {
            text: prompt.to_owned(),
        });
    }
    {
        let mut conversation = world.resource_mut::<Conversation>();
        conversation.last = Some((prompt.to_owned(), history.clone(), settings.clone()));
    }
    let run = spawn_run(world, agent, &history, prompt, settings.stream, None);
    world
        .entity_mut(run)
        .insert(crate::RunConfiguration(settings));
    {
        let mut conversation = world.resource_mut::<Conversation>();
        conversation.active = Some(run);
        conversation.runs = sequence;
    }
    // The run's program identity goes into the effect log under its scope:
    // every granted tool as a required row, so a replay advertises the same
    // tools even where the record never called them.
    world
        .entity_mut(run)
        .insert(rig_ecs::bus::Scope(format!("rigcoder/run/{sequence}")));
    let compatible = world.resource_scope(|world, setup: Mut<crate::Setup>| match &setup.mode {
        crate::Mode::Live => Ok(()),
        crate::Mode::Replay(log) => rig_ecs::replay::check_replayable(world, run, log),
    });
    if let Err(report) = compatible {
        world
            .entity_mut(run)
            .insert(Failed(rig_ecs::agent::Failure::Provider(report)));
        return Some(run);
    }
    if let Some(recorder) = world
        .get_resource::<rig_ecs::bus::EffectLogResource>()
        .map(|r| r.0.clone())
    {
        rig_ecs::replay::stamp_run(world, run, &recorder);
    }
    Some(run)
}

/// Stop the run in flight: `Cancelled` on the run ends it `Failed(Cancelled)`.
pub fn cancel(world: &mut World, reason: &str) {
    if world
        .resource_mut::<Conversation>()
        .retry_at
        .take()
        .is_some()
    {
        world.resource_mut::<Conversation>().last = None;
        world
            .resource_mut::<Transcript>()
            .push(Event::Failed(format!("cancelled: {reason}")));
    }
    if let Some(run) = world.resource::<Conversation>().active {
        world.entity_mut(run).insert(Cancelled(reason.to_owned()));
    }
}

/// A fresh tool child, seen in `Gate` before the bus dispatches it.
type UnansweredNewEffect = (Added<PendingEffect>, Without<EffectOutcome>);

pub fn announce_tool_calls(
    calls: Query<(&PendingEffect, &ToolCallSlot), UnansweredNewEffect>,
    mut transcript: ResMut<Transcript>,
) {
    for (effect, slot) in &calls {
        let args = match &effect.kind {
            EffectKind::ToolCall { args, .. } => args.clone(),
            other => other.name().to_owned(),
        };
        transcript.push(Event::ToolCall {
            name: slot.name.clone(),
            args,
        });
    }
}

/// A tool child's outcome, the moment the bus writes it.
pub fn announce_tool_results(
    answered: On<Add, EffectOutcome>,
    outcomes: Query<(&EffectOutcome, &ToolCallSlot)>,
    mut transcript: ResMut<Transcript>,
) {
    let Ok((outcome, slot)) = outcomes.get(answered.event().entity) else {
        return;
    };
    let (output, ok) = match &outcome.0 {
        Ok(Outcome::ToolResult { result }) => match result.error() {
            Some(error) => (format!("{error}"), false),
            None => (result.output().render(), true),
        },
        Ok(other) => (format!("unexpected {} outcome", other.family()), false),
        Err(report) => (format!("{report}"), false),
    };
    transcript.push(Event::ToolResult {
        name: slot.name.clone(),
        output,
        ok,
    });
}

/// Streamed text, the part that arrived since the last tick.
pub fn stream_text(
    streams: Query<(Entity, &Streamed), Changed<Streamed>>,
    mut conversation: ResMut<Conversation>,
    mut transcript: ResMut<Transcript>,
) {
    for (entity, stream) in &streams {
        let shown = conversation.shown.entry(entity).or_default();
        if stream.text.len() > *shown {
            transcript.append_text(&stream.text[*shown..]);
            *shown = stream.text.len();
        }
    }
}

pub fn on_settled(
    settled: On<Add, Settled>,
    results: Query<&RunResult>,
    usage: Query<&Usage>,
    utterances: Query<(&ChildOf, &Order, &Parts), With<Utterance>>,
    mut conversation: ResMut<Conversation>,
    mut transcript: ResMut<Transcript>,
) {
    let run = settled.event().entity;
    if conversation.active != Some(run) {
        return;
    }
    let answer = results.get(run).map(|r| r.0.clone()).unwrap_or_default();
    finish(run, &utterances, &mut conversation);
    // A non-streamed answer was never shown; a streamed one already was.
    if !matches!(transcript.events.last(), Some(Event::Assistant { .. })) && !answer.is_empty() {
        transcript.append_text(&answer);
    }
    record_usage(run, &usage, &mut transcript);
    transcript.push(Event::Settled { answer });
}

#[allow(clippy::too_many_arguments)] // Bevy injects independent event/query resources.
pub fn on_failed(
    failed: On<Add, Failed>,
    failures: Query<&Failed>,
    usage: Query<&Usage>,
    utterances: Query<(&ChildOf, &Order, &Parts), With<Utterance>>,
    mut conversation: ResMut<Conversation>,
    mut transcript: ResMut<Transcript>,
    setup: Res<crate::Setup>,
    settings: Query<&crate::RunConfiguration>,
) {
    let run = failed.event().entity;
    if conversation.active != Some(run) {
        return;
    }
    finish(run, &utterances, &mut conversation);
    record_usage(run, &usage, &mut transcript);
    let failure = failures
        .get(run)
        .ok()
        .map(|Failed(failure)| failure.clone());
    let reason = failure
        .as_ref()
        .map_or_else(|| "unknown".to_owned(), |f| format!("{f:?}"));
    // A whole-prompt retry is safe only before this request has produced
    // tool calls. Completed tools may have irreversible side effects, so a
    // later provider failure must preserve their history and end the run.
    if let Some(rig_ecs::agent::Failure::Provider(report)) = &failure
        && conversation.provider_retries < settings.get(run).map_or(0, |s| usize::from(s.0.provider_retries))
        && transient(report)
        && let Some((_, history, _)) = conversation.last.clone()
        && !conversation.history.iter().skip(history.len()).any(|parts| matches!(parts,
            MessageParts::Assistant { content, .. } if content.iter().any(|part| matches!(part, AssistantContent::ToolCall(_)))))
    {
        conversation.provider_retries += 1;
        // Replay delivery checks for missing requests at quiescence. The
        // recorded causal continuation must occur in this schedule pass;
        // wall-clock backoff is a live-provider concern.
        let wait = if matches!(setup.mode, crate::Mode::Replay(_)) {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs(1u64 << conversation.provider_retries.min(6))
        };
        conversation.history = history;
        conversation.retry_at = Some(std::time::Instant::now() + wait);
        transcript.push(Event::Retrying { reason: reason.clone(), attempt: conversation.provider_retries, wait_secs: wait.as_secs() });
        return;
    }
    transcript.push(Event::Failed(reason));
}

fn record_usage(run: Entity, usage: &Query<&Usage>, transcript: &mut Transcript) {
    if let Ok(Usage(wire)) = usage.get(run) {
        transcript.push(Event::Usage {
            input_tokens: wire.input_tokens,
            output_tokens: wire.output_tokens,
            cached_input_tokens: wire.cached_input_tokens,
            total_tokens: wire.total_tokens,
        });
    }
}

/// The run's utterances, in order, become the history; the run is over.
fn finish(
    run: Entity,
    utterances: &Query<(&ChildOf, &Order, &Parts), With<Utterance>>,
    conversation: &mut Conversation,
) {
    let mut parts: Vec<(u64, MessageParts)> = utterances
        .iter()
        .filter(|(child_of, _, _)| child_of.parent() == run)
        .map(|(_, order, parts)| (order.0, parts.0.clone()))
        .collect();
    parts.sort_by_key(|(order, _)| *order);
    conversation.history = parts.into_iter().map(|(_, parts)| parts).collect();
    conversation.active = None;
    conversation.shown.clear();
}

/// Plain text of a message, for rendering history.
pub fn message_text(parts: &MessageParts) -> String {
    match parts {
        MessageParts::User { content } => content
            .iter()
            .filter_map(|part| match part {
                UserContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        MessageParts::Assistant { content, .. } => content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// Does this provider report look like something a second attempt can fix?
fn transient(report: &rig::error::ErrorReport) -> bool {
    use rig::error::ErrorKind;
    // A replay divergence is a finding, never something to retry.
    if matches!(report.kind, ErrorKind::Divergence) {
        return false;
    }
    if report.retryable {
        return true;
    }
    if matches!(report.http_status, Some(429 | 500 | 502 | 503 | 504 | 529)) {
        return true;
    }
    let text = report.message.to_lowercase();
    [
        "stream ended",
        "terminal record",
        "rate limit",
        "overloaded",
        "timed out",
        "timeout",
        "connection reset",
        "connection closed",
        "eof",
        "temporarily",
    ]
    .iter()
    .any(|needle| text.contains(needle))
        || matches!(report.kind, ErrorKind::Timeout)
}

/// The exclusive step that submits a retried prompt once its backoff has
/// elapsed. Add it to `Update`.
pub fn resubmit_when_due(world: &mut World) {
    let due = {
        let conversation = world.resource::<Conversation>();
        conversation.active.is_none()
            && conversation
                .retry_at
                .is_some_and(|at| std::time::Instant::now() >= at)
    };
    if !due {
        return;
    }
    let (prompt, settings) = {
        let mut conversation = world.resource_mut::<Conversation>();
        conversation.retry_at = None;
        let Some((prompt, history, settings)) = conversation.last.clone() else {
            return;
        };
        conversation.history = history.clone();
        (prompt, settings)
    };
    if start_run(world, &prompt, false, settings).is_none() {
        world.resource_mut::<Transcript>().push(Event::Failed(
            "could not retry: no agent is registered".to_owned(),
        ));
    } else {
        world.resource_mut::<rig_ecs::bus::Progress>().mark();
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use bevy_app::{App, PreStartup};
    use rig::{
        completion::{CompletionResponse, ModelRef, ProviderCapabilities},
        effect::{FamilyDescriptor, HandlerDescriptor, HandlerKey},
        error::{ErrorKind, ErrorReport},
        serve::{Dispatch, Reply, Serve},
    };
    use rig_ecs::bus::Handlers;
    use std::{collections::VecDeque, sync::Mutex, time::Instant};

    type Answer = Result<Vec<AssistantContent>, ErrorReport>;
    struct Scripted(Mutex<VecDeque<Answer>>);
    impl Serve for Scripted {
        type Family = rig::effect::family::Completion;
        fn descriptor(&self) -> HandlerDescriptor {
            HandlerDescriptor {
                key: HandlerKey::from(crate::model::MODEL_KEY),
                family: FamilyDescriptor::Completion {
                    model: ModelRef::new("scripted"),
                    capabilities: ProviderCapabilities::default(),
                },
                layers: Vec::new(),
            }
        }
        async fn serve(&self, _: EffectKind, _dispatch: Dispatch) -> Reply {
            let answer = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra provider call");
            Reply::Outcome(answer.map(|content| {
                Outcome::Completion(CompletionResponse::new(
                    content,
                    rig::completion::Usage::new(),
                    "scripted",
                ))
            }))
        }
    }
    #[derive(Resource)]
    struct Model(Mutex<Option<Scripted>>);
    fn register(mut handlers: Handlers, model: Res<Model>) {
        if let Some(model) = model.0.lock().unwrap().take() {
            handlers.register(crate::model::MODEL_KEY, model).unwrap();
        }
    }
    fn app(dir: &std::path::Path, mode: crate::Mode, answers: Option<Vec<Answer>>) -> App {
        let mut app = App::new();
        app.add_plugins(crate::RigcoderPlugin {
            workspace: dir.to_owned(),
            model: crate::ModelChoice::parse("gemini", None).unwrap(),
            max_turns: 8,
            mode,
            prompt_override: None,
        });
        if let Some(answers) = answers {
            app.insert_resource(Model(Mutex::new(Some(Scripted(Mutex::new(
                answers.into(),
            ))))))
            .add_systems(PreStartup, register);
        }
        app.update();
        app
    }
    fn transient_failure() -> Answer {
        Err(ErrorReport::new(ErrorKind::Timeout, "timed out"))
    }
    fn done() -> Answer {
        Ok(vec![AssistantContent::text("done")])
    }
    fn drive(app: &mut App) {
        for _ in 0..2_000 {
            app.update();
            // Advance only the application's retry deadline; no wall-clock
            // backoff is needed to test the state transition deterministically.
            if app.world().resource::<Conversation>().has_pending_retry() {
                app.world_mut().resource_mut::<Conversation>().retry_at = Some(Instant::now());
            }
            if !app.world().resource::<Conversation>().is_busy() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("run did not terminate");
    }
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rigcoder-retry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    use std::path::PathBuf;

    fn settings(tokens: u64, stream: bool, retries: u8) -> crate::RunSettings {
        crate::RunSettings {
            max_tokens: tokens,
            stream,
            provider_retries: retries,
        }
    }

    #[test]
    fn automatic_retries_keep_the_original_configuration() {
        let dir = scratch("frozen-settings");
        let mut live = app(
            &dir,
            crate::Mode::Live,
            Some(vec![transient_failure(), transient_failure(), done()]),
        );
        live.insert_resource(settings(512, false, 1));
        submit(live.world_mut(), "first").unwrap();
        live.insert_resource(settings(1024, true, 3));
        drive(&mut live);
        let log = crate::effect_log(live.world());
        assert_eq!(log.records.len(), 2, "one original attempt and one retry");
        for record in log.iter() {
            let EffectKind::Completion { request, stream } = &record.kind else {
                panic!("expected completion");
            };
            assert_eq!(request.max_tokens, Some(512));
            assert!(!stream);
        }
        submit(live.world_mut(), "second").unwrap();
        drive(&mut live);
        let log = crate::effect_log(live.world());
        let EffectKind::Completion { request, stream } = &log.records.last().unwrap().kind else {
            panic!("expected completion");
        };
        assert_eq!(request.max_tokens, Some(1024));
        assert!(*stream);
    }

    #[test]
    fn differently_configured_runs_replay_from_one_log() {
        let dir = scratch("settings-replay");
        let mut live = app(&dir, crate::Mode::Live, Some(vec![done(), done()]));
        for (prompt, tokens) in [("first", 512), ("second", 1024)] {
            live.insert_resource(settings(tokens, false, 0));
            submit(live.world_mut(), prompt).unwrap();
            drive(&mut live);
        }
        let log = crate::effect_log(live.world());
        assert_eq!(log.header.programs.len(), 2);
        let mut replay = App::new();
        replay.insert_resource(settings(512, false, 0));
        let mut plugin =
            crate::RigcoderPlugin::live(dir, crate::ModelChoice::parse("gemini", None).unwrap(), 8);
        plugin.mode = crate::Mode::Replay(log.into());
        replay
            .add_plugins(plugin)
            .add_systems(bevy_app::PostStartup, |world: &mut World| {
                submit(world, "first").unwrap();
            });
        // Replay must reproduce subsequent host input before the bus declares
        // quiescence; otherwise the next recorded request is rightly missing.
        use bevy_ecs::schedule::IntoScheduleConfigs;
        replay.add_systems(
            rig_ecs::bus::RigSchedule,
            (|world: &mut World| {
                let conversation = world.resource::<Conversation>();
                if conversation.runs == 1 && !conversation.is_busy() {
                    world.insert_resource(settings(1024, false, 0));
                    submit(world, "second").unwrap();
                    world.resource_mut::<rig_ecs::bus::Progress>().mark();
                }
            })
            .after(rig_ecs::systems::RigSet::Settle),
        );
        drive(&mut replay);
        let events = &replay.world().resource::<Transcript>().events;
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Settled { .. }))
                .count(),
            2,
            "{events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::Failed(_))),
            "{events:?}"
        );
    }

    #[test]
    fn scene_retains_nondefault_configuration_and_run_sequence() {
        let dir = scratch("settings-scene");
        let mut live = app(&dir, crate::Mode::Live, Some(vec![done()]));
        live.insert_resource(settings(512, false, 1));
        live.world_mut().resource_mut::<Conversation>().runs = 6;
        submit(live.world_mut(), "first").unwrap();
        let scene = rig_ecs::agent::scene::save_world(live.world_mut()).unwrap();
        let scene = serde_json::from_slice(&serde_json::to_vec(&scene).unwrap()).unwrap();
        let mut resumed = app(&dir, crate::Mode::Live, Some(vec![done(), done()]));
        let run = crate::checkpoint::resume(resumed.world_mut(), &scene).unwrap();
        let restored = &resumed
            .world()
            .get::<crate::RunConfiguration>(run)
            .unwrap()
            .0;
        assert_eq!(restored.max_tokens, 512);
        assert!(!restored.stream);
        assert_eq!(restored.provider_retries, 1);
        assert_eq!(resumed.world().resource::<Conversation>().runs, 7);
        drive(&mut resumed);
        let next = submit(resumed.world_mut(), "next").unwrap();
        assert_eq!(
            resumed.world().get::<rig_ecs::bus::Scope>(next).unwrap().0,
            "rigcoder/run/8"
        );
    }

    #[test]
    fn retry_keeps_both_recorded_attempts_and_replays_them() {
        let dir = scratch("replay");
        let mut live = app(
            &dir,
            crate::Mode::Live,
            Some(vec![transient_failure(), done()]),
        );
        submit(live.world_mut(), "finish").unwrap();
        drive(&mut live);
        let log = crate::effect_log(live.world());
        assert_eq!(log.records.len(), 2);
        assert_eq!(live.world().resource::<Conversation>().runs, 2);
        let mut replay = App::new();
        replay
            .add_plugins(crate::RigcoderPlugin {
                workspace: dir,
                model: crate::ModelChoice::parse("gemini", None).unwrap(),
                max_turns: 8,
                mode: crate::Mode::Replay(log.into()),
                prompt_override: None,
            })
            .add_systems(bevy_app::PostStartup, |world: &mut World| {
                submit(world, "finish").unwrap();
            });
        drive(&mut replay);
        assert_eq!(
            replay.world().resource::<Conversation>().runs,
            2,
            "{:?}",
            replay.world().resource::<Transcript>().events
        );
        assert!(
            replay
                .world()
                .resource::<Transcript>()
                .events
                .iter()
                .any(|e| matches!(e, Event::Settled { answer } if answer == "done"))
        );
    }

    #[test]
    fn a_provider_failure_after_a_tool_is_not_retried_or_reexecuted() {
        let dir = scratch("side-effects");
        let command = Ok(vec![AssistantContent::tool_call(
            "append",
            "bash",
            serde_json::json!({"command": "printf x >> count.txt"}),
        )]);
        let mut app = app(
            &dir,
            crate::Mode::Live,
            Some(vec![
                command,
                transient_failure(),
                transient_failure(),
                done(),
            ]),
        );
        submit(app.world_mut(), "append once").unwrap();
        drive(&mut app);
        assert_eq!(std::fs::read_to_string(dir.join("count.txt")).unwrap(), "x");
        assert_eq!(app.world().resource::<Conversation>().runs, 1);
        assert!(
            !app.world()
                .resource::<Transcript>()
                .events
                .iter()
                .any(|e| matches!(e, Event::Retrying { .. }))
        );
        // Earlier conversation tools do not prevent a safe retry of a new
        // request that fails before doing tool work of its own.
        submit(app.world_mut(), "a new request").unwrap();
        drive(&mut app);
        assert_eq!(app.world().resource::<Conversation>().runs, 3);
        assert_eq!(std::fs::read_to_string(dir.join("count.txt")).unwrap(), "x");
    }

    #[test]
    fn a_backoff_is_busy_and_can_be_canceled_without_a_hidden_resubmission() {
        let dir = scratch("cancel");
        let mut app = app(&dir, crate::Mode::Live, Some(vec![transient_failure()]));
        submit(app.world_mut(), "first").unwrap();
        for _ in 0..2_000 {
            app.update();
            if app.world().resource::<Conversation>().has_pending_retry() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(app.world().resource::<Conversation>().has_pending_retry());
        assert!(submit(app.world_mut(), "second").is_none());
        cancel(app.world_mut(), "stop during backoff");
        resubmit_when_due(app.world_mut());
        assert!(!app.world().resource::<Conversation>().is_busy());
        assert_eq!(app.world().resource::<Conversation>().runs, 1);
        assert!(
            app.world().resource::<Transcript>().events.iter().any(
                |e| matches!(e, Event::Failed(reason) if reason.contains("stop during backoff"))
            )
        );
    }

    #[test]
    fn each_new_user_request_gets_its_own_retry_budget() {
        let dir = scratch("budget");
        let mut app = app(
            &dir,
            crate::Mode::Live,
            Some(vec![
                transient_failure(),
                done(),
                transient_failure(),
                done(),
            ]),
        );
        submit(app.world_mut(), "first").unwrap();
        drive(&mut app);
        submit(app.world_mut(), "second").unwrap();
        drive(&mut app);
        let attempts: Vec<_> = app
            .world()
            .resource::<Transcript>()
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Retrying { attempt, .. } => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(attempts, vec![1, 1]);
    }
}
