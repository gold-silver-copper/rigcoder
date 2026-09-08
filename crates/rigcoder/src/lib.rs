//! A coding agent over `rig-ecs`.
//!
//! The run is a Bevy world: the model is a handler entity, each tool is a
//! handler entity, the agent is an entity with `Grant` links to its tools,
//! and a prompt is a run entity spawned over that agent. Everything the UI
//! and the CLI show is read off components as the bus writes them; nothing
//! here awaits.

pub mod approval;
pub mod checkpoint;
mod file_change;
pub mod model;
pub mod observe;
pub mod session;
pub mod steer;
pub mod tools;

use std::{path::PathBuf, sync::Arc};

use bevy_app::{App, Plugin, Startup, Update};
use bevy_ecs::prelude::*;
use rig::serve::ServingPolicy;
use rig_ecs::{
    agent::scene::SceneExtensions,
    agent::{
        AdditionalParams, DefaultMaxTurns, InvalidCalls, MaxTokens, MaxTurns, Order, Output, Owner,
        PolicyVersion, Preamble, Temperature, ToolChoiceSpec, ToolPolicy, UsesModel,
    },
    bus::{
        Bound, BusSet, EffectLogResource, Handlers, Replay, RigSchedule, install_bus,
        run_to_quiescence,
    },
    prelude::*,
    systems::install_agent,
};

pub use model::ModelChoice;
pub use rig_effect_log::EffectLog;

/// The effect log recorded so far.
pub fn effect_log(world: &World) -> EffectLog {
    world.resource::<EffectLogResource>().log()
}
pub use observe::trace as observations;
pub use session::{AgentHandle, Conversation, Event, Transcript, cancel, submit};

/// The system prompt, kept as a file so the improvement harness can edit it
/// without touching Rust.
pub const SYSTEM_PROMPT: &str = include_str!("prompt.md");

/// Per-run provider settings shared by the CLI, UI and verification hosts.
/// Insert before setup, or change between runs. Zero retries disables automatic
/// whole-prompt retries; model/tool turn limits remain on `RigcoderPlugin`.
#[derive(Resource, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunSettings {
    pub stream: bool,
    pub max_tokens: u64,
    pub provider_retries: u8,
}

impl Default for RunSettings {
    fn default() -> Self {
        Self {
            stream: true,
            max_tokens: 16_000,
            provider_retries: session::MAX_PROVIDER_RETRIES as u8,
        }
    }
}

/// Settings frozen when a run is submitted, also retained by scene checkpoints.
#[derive(Component, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunConfiguration(pub RunSettings);

/// Where the agent works. Every relative tool path resolves against it and
/// every bash command starts in it.
#[derive(Resource, Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
}

/// Live: the provider model and the real tools. Replay: every recorded key
/// (the model, the tools) answered from an effect log, so a run replays on
/// a host with no keys and no side effects; the first request that differs
/// from its record fails the run with the divergence.
#[derive(Debug, Clone, Default)]
pub enum Mode {
    #[default]
    Live,
    Replay(Arc<EffectLog>),
}

/// The whole agent as one plugin: the bus, the agent runtime, the model and
/// tool handlers, the agent entity, and the systems that keep a transcript.
#[derive(Debug, Clone)]
pub struct RigcoderPlugin {
    pub workspace: PathBuf,
    pub model: ModelChoice,
    /// Model calls per run: the tool loop's budget.
    pub max_turns: usize,
    pub mode: Mode,
    /// Use this system prompt instead of the compiled-in `prompt.md`.
    pub prompt_override: Option<String>,
    /// Keep every streamed dispatch's events verbatim in the effect log
    /// (`EffectRecord::events`) instead of their fold. A completion that
    /// ends in an error, a cancellation or a truncation can then be
    /// classified from the frames the adapter saw, without a scripted
    /// reproduction. Costs the events' size per streamed exchange; the
    /// CLI turns it on whenever it writes an effect log.
    pub keep_stream_events: bool,
}

impl RigcoderPlugin {
    pub fn live(workspace: PathBuf, model: ModelChoice, max_turns: usize) -> Self {
        Self {
            workspace,
            model,
            max_turns,
            mode: Mode::Live,
            prompt_override: None,
            keep_stream_events: false,
        }
    }
}

impl Plugin for RigcoderPlugin {
    fn build(&self, app: &mut App) {
        let Self {
            workspace,
            model,
            max_turns,
            mode,
            prompt_override,
            keep_stream_events,
        } = self.clone();
        install_bus(app.world_mut(), ServingPolicy::default());
        install_agent(app.world_mut());
        app.add_systems(Update, run_to_quiescence);
        app.add_plugins(steer::SteerPlugin);
        // Every effect is recorded: the log replays on a host without keys.
        let recorder = if keep_stream_events {
            rig_effect_log::EffectLogRecorder::keeping_stream_events()
        } else {
            rig_effect_log::EffectLogRecorder::new()
        };
        EffectLogResource::install(app.world_mut(), recorder);
        // Every decision around those effects is witnessed: the trace is
        // the failure evidence beside the log.
        let observations = Arc::new(rig::observe::ObservationLog::default());
        rig_ecs::bus::Witnessing::install(app.world_mut(), observations.clone());
        app.insert_resource(observe::Observations(observations));
        app.insert_resource(Workspace { root: workspace })
            .init_resource::<RunSettings>()
            .insert_resource(model)
            .insert_resource(AgentBudget { max_turns })
            .insert_resource(Setup {
                mode,
                prompt_override,
            })
            .init_resource::<SceneExtensions>()
            .init_resource::<checkpoint::Checkpoint>()
            .init_resource::<checkpoint::MaterialisedTurns>()
            .add_observer(checkpoint::count_materialised)
            .add_systems(
                RigSchedule,
                checkpoint::save_between_turns
                    .after(RigSet::Materialise)
                    .before(RigSet::Settle),
            )
            .init_resource::<Conversation>()
            .init_resource::<Transcript>()
            .add_systems(Startup, (bind_replayers, setup).chain())
            .add_systems(
                RigSchedule,
                (
                    session::announce_tool_calls.in_set(BusSet::Gate),
                    session::stream_text.after(RigSet::Fold),
                ),
            )
            .add_systems(Update, session::resubmit_when_due.before(run_to_quiescence))
            .add_systems(
                RigSchedule,
                session::resubmit_when_due
                    .after(checkpoint::save_between_turns)
                    .before(RigSet::Settle),
            )
            .add_observer(session::announce_tool_results)
            .add_observer(session::on_settled)
            .add_observer(session::on_failed);
    }
}

#[derive(Resource, Debug, Clone, Copy)]
pub struct AgentBudget {
    max_turns: usize,
}

/// Register the model and the tools, spawn the agent, grant it every tool.
#[allow(clippy::too_many_arguments)] // Bevy injects these independent system parameters.
pub fn setup(
    mut handlers: Handlers,
    bound: Query<(Entity, &Bound)>,
    mut commands: Commands,
    workspace: Res<Workspace>,
    choice: Res<ModelChoice>,
    connection: Option<Res<model::ModelConnection>>,
    budget: Res<AgentBudget>,
    setup: Res<Setup>,
    mut transcript: ResMut<Transcript>,
    mut extensions: ResMut<SceneExtensions>,
) {
    let _ =
        extensions.register_component::<steer::DeliverableRetries>("rigcoder.deliverable_retries");
    let _ = extensions.register_component::<RunConfiguration>("rigcoder.run_settings");
    let _ = extensions.register_component::<approval::RunApproval>("rigcoder.run_approval");
    let (model, tools) = match &setup.mode {
        Mode::Replay(_) => {
            let model = bound
                .iter()
                .find(|(_, b)| b.key.as_str() == model::MODEL_KEY)
                .map(|(e, _)| e);
            let Some(model) = model else {
                transcript.push(Event::Failed {
                    reason: "the effect log records no model exchange".to_owned(),
                });
                return;
            };
            let mut tools: Vec<(usize, String, Entity)> = bound
                .iter()
                .filter(|(_, b)| b.key.as_str().starts_with("tool:"))
                .map(|(e, b)| {
                    let name = &b.key.as_str()["tool:".len()..];
                    let rank = tools::NAMES
                        .iter()
                        .position(|n| *n == name)
                        .unwrap_or(tools::NAMES.len());
                    (rank, name.to_owned(), e)
                })
                .collect();
            tools.sort();
            (model, tools.into_iter().map(|(_, _, e)| e).collect())
        }
        Mode::Live => {
            // A model already registered under the key (a test's scripted one)
            // is used as it is; otherwise the provider's is registered.
            let existing = bound
                .iter()
                .find(|(_, b)| b.key.as_str() == model::MODEL_KEY)
                .map(|(e, _)| e);
            let model = match existing.map(Ok).unwrap_or_else(|| {
                model::register_with_connection(&mut handlers, &choice, connection.as_deref())
            }) {
                Ok(model) => model,
                Err(report) => {
                    transcript.push(Event::Failed {
                        reason: format!("could not register the model: {report}"),
                    });
                    return;
                }
            };
            (model, tools::register_all(&mut handlers, &workspace.root))
        }
    };
    let prompt = setup.prompt_override.as_deref().unwrap_or(SYSTEM_PROMPT);
    let preamble = format!(
        "{prompt}\nWorkspace directory: {}\n",
        workspace.root.display()
    );
    let agent = commands
        .spawn((
            Owner("rigcoder".to_owned()),
            Preamble(Some(preamble)),
            Temperature(None),
            MaxTokens(Some(16_000)),
            AdditionalParams(None),
            ToolChoiceSpec(None),
            Output::default(),
            DefaultMaxTurns(Some(budget.max_turns)),
            MaxTurns(budget.max_turns),
            InvalidCalls::default(),
            ToolPolicy { concurrency: 4 },
            // Replay identity: the systems and settings this crate adds beyond
            // the graph, named so a log knows what it was recorded under.
            PolicyVersion(format!("rigcoder-{}", env!("CARGO_PKG_VERSION"))),
            UsesModel(model),
        ))
        .id();
    for (order, tool) in tools.iter().enumerate() {
        commands.spawn((Grant(*tool), Order(order as u64), ChildOf(agent)));
    }
    commands.insert_resource(AgentHandle {
        agent,
        model,
        tools,
    });
}

/// How `setup` binds the model and the tools, and which system prompt it
/// gives the agent.
#[derive(Resource, Debug, Clone, Default)]
pub struct Setup {
    pub mode: Mode,
    pub prompt_override: Option<String>,
}

/// In replay mode, bind a replayer for every recorded key before `setup`
/// looks the handler entities up (commands apply between the two).
fn bind_replayers(mut handlers: Handlers, setup: Res<Setup>, mut transcript: ResMut<Transcript>) {
    if let Mode::Replay(log) = &setup.mode
        && let Err(report) = Replay::default().register(&mut handlers, log)
    {
        transcript.push(Event::Failed {
            reason: format!("could not bind the replayers: {report}"),
        });
    }
}
