//! A coding agent over `rig-ecs`.
//!
//! The run is a Bevy world: the model is a handler entity, each tool is a
//! handler entity, the agent is an entity with `Grant` links to its tools,
//! and a prompt is a run entity spawned over that agent. Everything the UI
//! and the CLI show is read off components as the bus writes them; nothing
//! here awaits.

pub mod model;
pub mod session;
pub mod steer;
pub mod tools;

use std::path::PathBuf;

use bevy_app::{App, Plugin, Startup};
use bevy_ecs::prelude::*;
use rig_ecs::{
    agent::{
        AdditionalParams, DefaultMaxTurns, InvalidCalls, MaxTokens, MaxTurns, Order, Output,
        Owner, Preamble, Temperature, ToolChoiceSpec, ToolPolicy, UsesModel,
    },
    bus::{BusPlugin, BusSet, Handlers, RigSchedule},
    prelude::*,
    systems::AgentPlugin,
};

pub use model::ModelChoice;
pub use session::{AgentHandle, Conversation, Event, Transcript, cancel, submit};

/// The system prompt, kept as a file so the improvement harness can edit it
/// without touching Rust.
pub const SYSTEM_PROMPT: &str = include_str!("prompt.md");

/// Where the agent works. Every relative tool path resolves against it and
/// every bash command starts in it.
#[derive(Resource, Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
}

/// The whole agent as one plugin: the bus, the agent runtime, the model and
/// tool handlers, the agent entity, and the systems that keep a transcript.
#[derive(Debug, Clone)]
pub struct RigcoderPlugin {
    pub workspace: PathBuf,
    pub model: ModelChoice,
    /// Model calls per run: the tool loop's budget.
    pub max_turns: usize,
}

impl Plugin for RigcoderPlugin {
    fn build(&self, app: &mut App) {
        let Self {
            workspace,
            model,
            max_turns,
        } = self.clone();
        app.add_plugins((BusPlugin::default(), AgentPlugin::default(), steer::SteerPlugin))
            .insert_resource(Workspace { root: workspace })
            .insert_resource(model)
            .insert_resource(AgentBudget { max_turns })
            .init_resource::<Conversation>()
            .init_resource::<Transcript>()
            .add_systems(Startup, setup)
            .add_systems(
                RigSchedule,
                (
                    session::announce_tool_calls.in_set(BusSet::Gate),
                    session::stream_text.after(RigSet::Fold),
                ),
            )
            .add_observer(session::announce_tool_results)
            .add_observer(session::on_settled)
            .add_observer(session::on_failed);
    }
}

#[derive(Resource, Debug, Clone, Copy)]
struct AgentBudget {
    max_turns: usize,
}

/// Register the model and the tools, spawn the agent, grant it every tool.
fn setup(
    mut handlers: Handlers,
    mut commands: Commands,
    workspace: Res<Workspace>,
    choice: Res<ModelChoice>,
    budget: Res<AgentBudget>,
    mut transcript: ResMut<Transcript>,
) {
    let model = match model::register(&mut handlers, &choice) {
        Ok(model) => model,
        Err(report) => {
            transcript.push(Event::Failed(format!("could not register the model: {report}")));
            return;
        }
    };
    let tools = tools::register_all(&mut handlers, &workspace.root);
    let preamble = format!(
        "{SYSTEM_PROMPT}\nWorkspace directory: {}\n",
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
