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
    agent::{
        MessageParts, Order, Parts, ProviderRetried, ProviderRetries, ProviderRetrying, RunResult,
        ToolCallSlot, Usage, Utterance,
    },
    bus::{Held, PendingEffect},
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
}

/// The run's initial completion identity, runtime-only: every attempt of
/// the run's first completion (the runtime's provider retries, CONTRACT
/// §5) is stamped with it and its one-based host attempt, so the adapter's
/// facts correlate across attempts. Never inherited by later tool-result
/// turns: those are new operations.
#[derive(Component)]
pub(crate) struct ProviderOperation(rig::observe::AdapterContext);

pub(crate) fn correlate_provider_attempts(
    mut commands: Commands,
    effects: Query<(Entity, &PendingEffect, &ChildOf), Added<PendingEffect>>,
    parents: Query<&ChildOf>,
    runs: Query<(
        &ProviderOperation,
        &ProviderRetried,
        &rig_ecs::agent::Cursor,
    )>,
) {
    for (entity, effect, turn) in &effects {
        if !matches!(effect.kind, EffectKind::Completion { .. }) {
            continue;
        }
        let Ok(run) = parents.get(turn.parent()) else {
            continue;
        };
        let Ok((operation, retried, cursor)) = runs.get(run.parent()) else {
            continue;
        };
        // The first model call is turn 1; a retry re-issues it without
        // advancing the cursor, so the cursor names the initial completion
        // for as long as its attempts last.
        if cursor.turn != 1 {
            continue;
        }
        let Some(host_attempt) = u64::try_from(retried.0)
            .ok()
            .and_then(|spent| spent.checked_add(1))
            .and_then(std::num::NonZeroU64::new)
        else {
            continue;
        };
        commands
            .entity(entity)
            .insert(rig_ecs::bus::AdapterOperation {
                context: operation.0.clone(),
                host_attempt,
            });
    }
}

/// Transient provider failures are retried this many times, with backoff:
/// the run's `ProviderRetries` budget (CONTRACT §5).
pub const MAX_PROVIDER_RETRIES: usize = rig_ecs::agent::DEFAULT_PROVIDER_RETRIES;

impl Conversation {
    /// A run is in flight, its provider backoff included.
    pub fn is_busy(&self) -> bool {
        self.active.is_some()
    }
}

/// The deadline a re-issued completion waits for before dispatch: the
/// host's backoff, as a hold on the effect (CONTRACT §5).
#[derive(Component, Debug, Clone, Copy)]
pub struct RetryBackoff(pub std::time::Instant);

/// Exponential backoff for the `attempt`th retry, capped at 64 seconds.
fn backoff(attempt: usize) -> std::time::Duration {
    std::time::Duration::from_secs(1u64 << attempt.min(6))
}

/// Hold a retried completion for its backoff, live only: replay delivery
/// checks for missing requests at quiescence, so a recorded continuation
/// must not wait on the wall clock.
pub(crate) fn hold_retried_completions(
    mut commands: Commands,
    effects: Query<(Entity, &PendingEffect, &ChildOf), Added<PendingEffect>>,
    parents: Query<&ChildOf>,
    runs: Query<&ProviderRetried>,
    setup: Res<crate::Setup>,
) {
    if matches!(setup.mode, crate::Mode::Replay(_)) {
        return;
    }
    for (entity, effect, turn) in &effects {
        if !matches!(effect.kind, EffectKind::Completion { .. }) {
            continue;
        }
        let Ok(run) = parents.get(turn.parent()) else {
            continue;
        };
        let Ok(retried) = runs.get(run.parent()) else {
            continue;
        };
        if retried.0 == 0 {
            continue;
        }
        commands.entity(entity).insert((
            Held,
            RetryBackoff(std::time::Instant::now() + backoff(retried.0)),
        ));
    }
}

/// Release every backoff hold that is due. Add it to `Update`, before the
/// runner, so a due retry dispatches on this tick.
pub fn release_backoffs(world: &mut World) {
    let now = std::time::Instant::now();
    let due: Vec<Entity> = world
        .query::<(Entity, &RetryBackoff)>()
        .iter(world)
        .filter(|(_, deadline)| now >= deadline.0)
        .map(|(entity, _)| entity)
        .collect();
    if due.is_empty() {
        return;
    }
    for entity in due {
        world.entity_mut(entity).remove::<(Held, RetryBackoff)>();
    }
    world.resource_mut::<rig_ecs::bus::Progress>().mark();
}

/// Make every pending backoff due now: a host that already waited (or a
/// test that must not) dispatches it on the next tick.
pub fn expire_backoffs(world: &mut World) {
    let now = std::time::Instant::now();
    let pending: Vec<Entity> = world
        .query_filtered::<Entity, With<RetryBackoff>>()
        .iter(world)
        .collect();
    for entity in pending {
        world.entity_mut(entity).insert(RetryBackoff(now));
    }
}

/// A run whose completion the runtime just re-issued.
type JustRetried = (With<ProviderRetrying>, Changed<ProviderRetried>);

/// The runtime re-issued the active run's completion (CONTRACT §5): the
/// transcript says so, with the failure it retries and the backoff.
pub(crate) fn announce_provider_retries(
    runs: Query<(Entity, &ProviderRetried), JustRetried>,
    turns: Query<&ChildOf, With<rig_ecs::agent::Turn>>,
    effects: Query<(
        &ChildOf,
        &rig_ecs::bus::Seq,
        &PendingEffect,
        &rig_ecs::bus::EffectOutcome,
    )>,
    conversation: Res<Conversation>,
    setup: Res<crate::Setup>,
    connection: Option<Res<crate::model::ModelConnection>>,
    mut transcript: ResMut<Transcript>,
) {
    for (run, retried) in &runs {
        if conversation.active != Some(run) || retried.0 == 0 {
            continue;
        }
        // Effects carry the bus's Seq, not the agent graph's Order (which
        // orders utterances and links). Select only this run's completions.
        let reason = effects
            .iter()
            .filter(|(turn, _, effect, _)| {
                matches!(effect.kind, EffectKind::Completion { .. })
                    && turns.get(turn.parent()).is_ok_and(|of| of.parent() == run)
            })
            .filter_map(|(_, order, _, outcome)| match &outcome.0 {
                Err(report) => Some((order.0, report)),
                Ok(_) => None,
            })
            .max_by_key(|(order, _)| *order)
            .map(|(_, report)| {
                let secrets = crate::model::diagnostic_secrets(
                    connection.as_deref(),
                    Some(&setup.diagnostic_secrets),
                );
                rig::observe::scrub_diagnostic(&report.message, &secrets)
            })
            .unwrap_or_default();
        let wait = if matches!(setup.mode, crate::Mode::Replay(_)) {
            std::time::Duration::ZERO
        } else {
            backoff(retried.0)
        };
        transcript.push(Event::Retrying {
            reason,
            attempt: retried.0,
            wait_secs: wait.as_secs(),
        });
    }
}

/// Runtime marker for a failed pre-dispatch replay compatibility check.
/// Keeps its known origin without changing Rig's error kind or retry policy.
#[derive(Component)]
pub struct ReplayRejected;

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
    /// The run ended without settling: the reason, as the runtime reported
    /// it. A struct variant, because an internally tagged newtype of a
    /// string cannot be serialized, which silently dropped every ending
    /// from `transcript.jsonl`.
    Failed {
        reason: crate::failure::FailureDetail,
    },
    /// The runtime re-issued the completion after a transient provider
    /// failure (CONTRACT §5): no tool is re-run, no history rewritten.
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
    let settings = world.resource::<crate::RunSettings>().clone();
    let approval = *world.resource::<crate::approval::ApprovalMode>();
    start_run(world, prompt, settings, approval)
}

fn start_run(
    world: &mut World,
    prompt: &str,
    settings: crate::RunSettings,
    approval: crate::approval::ApprovalMode,
) -> Option<Entity> {
    let agent = world.get_resource::<AgentHandle>()?.agent;
    if world.resource::<Conversation>().active.is_some() {
        return None;
    }
    if settings.max_tokens == 0 {
        crate::failure::record_world(
            world,
            rig::observe::Subject::default(),
            "session",
            crate::failure::FailureDetail::host(
                "invalid_configuration",
                "max_tokens must be greater than zero",
                &[],
            ),
        );
        return None;
    }
    let Some(sequence) = world.resource::<Conversation>().runs.checked_add(1) else {
        crate::failure::record_world(
            world,
            rig::observe::Subject::default(),
            "session",
            crate::failure::FailureDetail::host(
                "identity_exhausted",
                "run sequence exhausted",
                &[],
            ),
        );
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
        include_str!("approval.rs"),
        include_str!("steer.rs"),
        include_str!("session.rs"),
        &settings,
        approval,
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
    world.resource_mut::<Transcript>().push(Event::User {
        text: prompt.to_owned(),
    });
    let run = spawn_run(world, agent, &history, prompt, settings.stream, None);
    if let Some(context) = world
        .get_resource::<rig_ecs::bus::Witnessing>()
        .map(|witness| {
            rig::observe::AdapterContext::new(
                witness.sink().clone(),
                rig::observe::Subject::default(),
                format!("rigcoder/request/{sequence}/completion/0"),
            )
        })
    {
        world.entity_mut(run).insert(ProviderOperation(context));
    }
    world.entity_mut(run).insert((
        ProviderRetries(usize::from(settings.provider_retries)),
        crate::RunConfiguration(settings),
        crate::approval::RunApproval(approval),
    ));
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
        world.entity_mut(run).insert(ReplayRejected);
        world
            .entity_mut(run)
            .insert(Failed(rig_ecs::agent::Failure::Provider(report)));
        return Some(run);
    }
    if let Some(recorder) = world
        .get_resource::<rig_ecs::bus::EffectLogResource>()
        .map(|r| r.0.clone())
    {
        // A log stamped without a verifiable identity could never replay;
        // the run fails here, named, rather than at replay time.
        if let Err(report) = rig_ecs::replay::stamp_run(world, run, &recorder) {
            world
                .entity_mut(run)
                .insert(Failed(rig_ecs::agent::Failure::Unsupported(format!(
                    "cannot stamp program identity: {report}"
                ))));
        }
    }
    Some(run)
}

/// Stop the run in flight: `Cancelled` on the run ends it `Failed(Cancelled)`.
pub fn cancel(world: &mut World, reason: &str) {
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
    failures: Query<(&Failed, Has<ReplayRejected>)>,
    usage: Query<&Usage>,
    utterances: Query<(&ChildOf, &Order, &Parts), With<Utterance>>,
    mut conversation: ResMut<Conversation>,
    mut transcript: ResMut<Transcript>,
    setup: Res<crate::Setup>,
    witness: Option<Res<rig_ecs::bus::Witnessing>>,
    scopes: Query<&rig_ecs::bus::Scope>,
    observations: Option<Res<crate::observe::Observations>>,
    connection: Option<Res<crate::model::ModelConnection>>,
    mut commands: Commands,
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
        .map(|(Failed(failure), _)| failure.clone());
    let secrets =
        crate::model::diagnostic_secrets(connection.as_deref(), Some(&setup.diagnostic_secrets));
    let mut reason = crate::failure::FailureDetail::runtime(failure.as_ref(), &secrets);
    if failures.get(run).is_ok_and(|(_, rejected)| rejected) {
        reason.origin = "replay_validation".into();
        reason.boundary = crate::failure::FailureBoundary::Host;
    }
    if let (Ok(scope), Some(observations)) = (scopes.get(run), observations.as_ref()) {
        reason.attach(&scope.0, &observations.0.trace());
    }
    let subject = scopes.get(run).map_or_else(
        |_| rig::observe::Subject::default(),
        |scope| rig::observe::Subject::scoped(scope.0.clone()),
    );
    // Lifecycle observers have no ordering guarantee. Emit host diagnostics
    // after they finish, so the owning runtime's Ended fact comes first.
    if let Some(witness) = witness.as_deref().cloned() {
        let subject = subject.clone();
        let reason = reason.clone();
        commands.queue(move |_: &mut World| {
            crate::observe::emit(Some(&witness), subject, "session", &reason);
        });
    }
    transcript.push(Event::Failed { reason });
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
    use std::{collections::VecDeque, sync::Mutex};

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
            keep_stream_events: false,
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
    /// A typed retryable report: the runtime retries on `retryable`, not
    /// on the message.
    fn transient_failure() -> Answer {
        Err(ErrorReport::new(ErrorKind::Timeout, "timed out").with_retryable(true))
    }
    fn done() -> Answer {
        Ok(vec![AssistantContent::text("done")])
    }
    fn drive(app: &mut App) {
        for _ in 0..2_000 {
            app.update();
            // The backoff is a hold on the re-issued effect; make it due so
            // the state transition is tested, not the wall clock.
            expire_backoffs(app.world_mut());
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

    fn retries(app: &App) -> Vec<usize> {
        app.world()
            .resource::<Transcript>()
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Retrying { attempt, .. } => Some(*attempt),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn retry_transcripts_name_each_failed_completion() {
        let dir = scratch("retry-reasons");
        let mut live = app(
            &dir,
            crate::Mode::Live,
            Some(vec![
                Err(ErrorReport::new(ErrorKind::Timeout, "first timeout").with_retryable(true)),
                Err(ErrorReport::new(ErrorKind::Timeout, "second timeout").with_retryable(true)),
                Err(
                    ErrorReport::new(ErrorKind::Timeout, "echo synthetic-retry-secret")
                        .with_retryable(true),
                ),
                done(),
            ]),
        );
        live.insert_resource(crate::model::ModelConnection::new(
            "https://example.invalid",
            "synthetic-retry-secret",
            rig::http_client::ReqwestClient::new(reqwest::Client::new()).boxed(),
        ));
        submit(live.world_mut(), "finish").unwrap();
        drive(&mut live);
        let reasons: Vec<_> = live
            .world()
            .resource::<Transcript>()
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Retrying { reason, .. } => Some(reason.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasons.len(), 3);
        assert_eq!(&reasons[..2], ["first timeout", "second timeout"]);
        assert!(!reasons[2].is_empty());
        assert!(!reasons[2].contains("synthetic-retry-secret"));
    }

    fn held_backoffs(app: &mut App) -> usize {
        app.world_mut()
            .query_filtered::<Entity, (With<Held>, With<RetryBackoff>)>()
            .iter(app.world())
            .count()
    }

    #[test]
    fn every_event_kind_serializes_as_a_transcript_line() {
        // A `failed` line was silently missing from every transcript: an
        // internally tagged newtype variant of a string does not serialize.
        for event in [
            Event::Failed {
                reason: crate::failure::FailureDetail::host("cancelled", "cancelled", &[]),
            },
            Event::Settled {
                answer: "ok".into(),
            },
            Event::Denied {
                name: "bash".into(),
                reason: "no".into(),
            },
        ] {
            let line = serde_json::to_string(&event).expect("every event is a line");
            assert!(line.contains("\"kind\":"), "{line}");
        }
        let event = serde_json::to_value(Event::Failed {
            reason: crate::failure::FailureDetail::host("cancelled", "operator stop", &[]),
        })
        .unwrap();
        assert_eq!(event["kind"], "failed");
        assert_eq!(event["reason"]["kind"], "cancelled");
        assert_eq!(event["reason"]["message"], "operator stop");
        assert!(event["reason"]["adapter"].is_null());
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
        live.insert_resource(crate::approval::ApprovalMode::Ask);
        submit(live.world_mut(), "first").unwrap();
        live.insert_resource(settings(1024, true, 3));
        live.insert_resource(crate::approval::ApprovalMode::Auto);
        drive(&mut live);
        assert!(
            live.world_mut()
                .query::<&crate::approval::RunApproval>()
                .iter(live.world())
                .all(|mode| mode.0 == crate::approval::ApprovalMode::Ask)
        );
        let log = crate::effect_log(live.world());
        assert_eq!(log.records.len(), 2, "one original attempt and one retry");
        for record in log.iter() {
            let EffectKind::Completion { request, stream } = &record.kind else {
                panic!("expected completion");
            };
            assert_eq!(request.max_tokens, Some(512));
            assert!(!stream);
        }
        // The budget of one was the run's, set when it was submitted.
        assert_eq!(retries(&live), [1]);
        assert!(
            live.world()
                .resource::<Transcript>()
                .events
                .iter()
                .any(|e| matches!(e, Event::Failed { reason } if reason.kind == "timeout"))
        );
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
            !events.iter().any(|e| matches!(e, Event::Failed { .. })),
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
        // The run's budget travels with the scene as the library's setting.
        assert_eq!(
            resumed.world().get::<ProviderRetries>(run).map(|r| r.0),
            Some(1)
        );
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
        assert_eq!(log.records.len(), 2, "the lost attempt and the retry");
        // One run: the retry is the run's, not a second submission.
        assert_eq!(live.world().resource::<Conversation>().runs, 1);
        assert_eq!(retries(&live), [1]);
        let mut replay = App::new();
        replay
            .add_plugins(crate::RigcoderPlugin {
                workspace: dir,
                model: crate::ModelChoice::parse("gemini", None).unwrap(),
                max_turns: 8,
                mode: crate::Mode::Replay(log.into()),
                prompt_override: None,
                keep_stream_events: false,
            })
            .add_systems(bevy_app::PostStartup, |world: &mut World| {
                submit(world, "finish").unwrap();
            });
        drive(&mut replay);
        assert_eq!(
            replay.world().resource::<Conversation>().runs,
            1,
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
        assert_eq!(
            retries(&replay),
            [1],
            "the replay retries where the log did"
        );
        for app in [&live, &replay] {
            assert!(app.world().resource::<Transcript>().events.iter().any(
                |event| matches!(event, Event::Retrying { reason, .. } if reason == "timed out")
            ));
        }
    }

    #[test]
    fn a_provider_failure_after_a_tool_is_retried_without_reexecuting_it() {
        // Before the run-level retry a provider failure after tool work was
        // fatal, because the only retry resubmitted the whole prompt. Now
        // the completion is re-issued over the history the tool result is
        // in: the tool ran once and the run finishes.
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
        assert_eq!(retries(&app), [1, 2]);
        assert!(
            app.world()
                .resource::<Transcript>()
                .events
                .iter()
                .any(|e| matches!(e, Event::Settled { answer } if answer == "done"))
        );
        let log = crate::effect_log(app.world());
        let tools = log
            .iter()
            .filter(|r| matches!(r.kind, EffectKind::ToolCall { .. }))
            .count();
        assert_eq!(tools, 1, "the tool is in the log once");
    }

    #[test]
    fn a_backoff_is_a_hold_and_a_cancel_during_it_ends_the_run() {
        let dir = scratch("cancel");
        let mut app = app(&dir, crate::Mode::Live, Some(vec![transient_failure()]));
        submit(app.world_mut(), "first").unwrap();
        for _ in 0..2_000 {
            app.update();
            if held_backoffs(&mut app) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(held_backoffs(&mut app), 1, "the re-issued completion waits");
        assert!(app.world().resource::<Conversation>().is_busy());
        assert!(submit(app.world_mut(), "second").is_none());
        assert_eq!(retries(&app), [1]);
        cancel(app.world_mut(), "stop during backoff");
        drive(&mut app);
        assert!(!app.world().resource::<Conversation>().is_busy());
        assert_eq!(app.world().resource::<Conversation>().runs, 1);
        assert_eq!(held_backoffs(&mut app), 0, "the held attempt is gone");
        assert!(app.world().resource::<Transcript>().events.iter().any(
            |e| matches!(e, Event::Failed { reason } if reason.kind == "cancelled" && reason.message == "stop during backoff")
        ));
        use rig::observe::HostAction as _;
        let cancellations: Vec<_> = crate::observations(app.world())
            .unwrap()
            .observations
            .iter()
            .filter_map(|observation| {
                crate::failure::FailureDetail::from_action(&observation.action)
            })
            .map(Result::unwrap)
            .filter(|failure| failure.kind == "cancelled")
            .collect();
        assert_eq!(cancellations.len(), 1);
        assert_eq!(cancellations[0].message, "stop during backoff");
        // The held attempt never went out: one record, the lost one.
        assert_eq!(crate::effect_log(app.world()).records.len(), 1);
    }

    #[test]
    fn a_non_retryable_failure_is_not_retried() {
        let dir = scratch("refusal");
        let mut app = app(
            &dir,
            crate::Mode::Live,
            Some(vec![
                Err(ErrorReport::new(ErrorKind::Provider, "blocked: SAFETY")),
                done(),
            ]),
        );
        submit(app.world_mut(), "first").unwrap();
        drive(&mut app);
        assert!(retries(&app).is_empty());
        assert!(
            app.world().resource::<Transcript>().events.iter().any(
                |e| matches!(e, Event::Failed { reason } if reason.message == "blocked: SAFETY")
            )
        );
        assert_eq!(crate::effect_log(app.world()).records.len(), 1);
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
        assert_eq!(retries(&app), vec![1, 1]);
    }
}
