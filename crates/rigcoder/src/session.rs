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
}

/// One thing that happened, in the order it happened.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    User { text: String },
    /// Assistant text; a streaming answer grows the last one of these.
    Assistant { text: String },
    ToolCall { name: String, args: String },
    ToolResult { name: String, output: String, ok: bool },
    Settled { answer: String },
    Failed(String),
    /// A tool call the gate refused; the model reads the reason as the result.
    Denied { name: String, reason: String },
    /// A tool call waiting for approval.
    Held { name: String, args: String },
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
    let agent = world.get_resource::<AgentHandle>()?.agent;
    if world.resource::<Conversation>().active.is_some() {
        return None;
    }
    let history = world.resource::<Conversation>().history.clone();
    world.resource_mut::<Transcript>().push(Event::User {
        text: prompt.to_owned(),
    });
    let run = spawn_run(world, agent, &history, prompt, true, None);
    let mut conversation = world.resource_mut::<Conversation>();
    conversation.active = Some(run);
    conversation.runs += 1;
    Some(run)
}

/// Stop the run in flight: `Cancelled` on the run ends it `Failed(Cancelled)`.
pub fn cancel(world: &mut World, reason: &str) {
    if let Some(run) = world.resource::<Conversation>().active {
        world.entity_mut(run).insert(Cancelled(reason.to_owned()));
    }
}

/// A fresh tool child, seen in `Gate` before the bus dispatches it.
pub fn announce_tool_calls(
    calls: Query<(&PendingEffect, &ToolCallSlot), Added<PendingEffect>>,
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

pub fn on_failed(
    failed: On<Add, Failed>,
    failures: Query<&Failed>,
    usage: Query<&Usage>,
    utterances: Query<(&ChildOf, &Order, &Parts), With<Utterance>>,
    mut conversation: ResMut<Conversation>,
    mut transcript: ResMut<Transcript>,
) {
    let run = failed.event().entity;
    if conversation.active != Some(run) {
        return;
    }
    finish(run, &utterances, &mut conversation);
    record_usage(run, &usage, &mut transcript);
    let reason = failures
        .get(run)
        .map(|Failed(failure)| format!("{failure:?}"))
        .unwrap_or_else(|_| "unknown".to_owned());
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
