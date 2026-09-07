//! Steering systems: what a hook would be, as systems on the bus's sets.
//!
//! `BusSet::Gate` sees a tool call before dispatch: a bash command on the
//! deny list is answered `Denied` with a reason the model reads, one on the
//! hold list waits (`Held`) for an approval. `BusSet::Judge` sees a tool
//! outcome before history reads it: an over-long result is cut to head and
//! tail while the record keeps the full answer. `RigSet::Judge` sees a
//! turn's outputs before they materialise: a text-only answer while a
//! required file is missing becomes a `Retry` naming the files.
//!
//! The rules live in the [`Steer`] resource, so the settings lane tunes
//! them and the systems lane changes the systems.

use std::{collections::VecDeque, path::PathBuf};

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;
use rig::{
    effect::{EffectKind, Outcome},
    error::{ErrorKind, ErrorReport},
    message::AssistantContent,
    tool::{ToolOutput, ToolResult},
};
use rig_ecs::{
    agent::{Outputs, Retry, RunOf, ToolCallSlot, Turn},
    bus::{BusSet, EffectOutcome, Held, Issued, PendingEffect, RigSchedule},
    systems::{Materialised, RigSet},
};

use crate::session::{Event, Transcript};

/// The tunable rules.
#[derive(Resource, Debug, Clone)]
pub struct Steer {
    /// Regexes; a bash command matching one is denied with the reason.
    pub deny: Vec<(String, String)>,
    /// Regexes; a bash command matching one is held until approved.
    pub hold: Vec<String>,
    /// Held commands are approved as soon as they are logged (headless).
    pub auto_approve: bool,
    /// Tool results longer than this reach history as head and tail.
    pub max_result_chars: usize,
    /// Files the task requires; a text-only answer while one is missing is
    /// retried with feedback, up to `max_deliverable_retries` per run.
    pub deliverables: Vec<PathBuf>,
    pub max_deliverable_retries: usize,
}

impl Default for Steer {
    fn default() -> Self {
        Self {
            deny: vec![
                (r"(^|[;&|]\s*)(find|grep|rg|ls)\s+(-\S+\s+)*/(\s|$)".to_owned(), "searching the whole filesystem hangs the run; search inside the workspace".to_owned()),
                (r"(^|[;&|]\s*)grep\s+-[a-zA-Z]*r[a-zA-Z]*\s+.*\s/(\s|$)".to_owned(), "a recursive grep of / hangs the run; search inside the workspace".to_owned()),
                (r"rm\s+(-\S+\s+)*-?[a-zA-Z]*[rR][a-zA-Z]*\s+/(\s|$)".to_owned(), "refusing to delete /".to_owned()),
                (r"(^|[;&|]\s*)(shutdown|reboot|halt|poweroff)(\s|$)".to_owned(), "no".to_owned()),
            ],
            hold: Vec::new(),
            auto_approve: true,
            max_result_chars: 30_000,
            deliverables: Vec::new(),
            max_deliverable_retries: 2,
        }
    }
}

/// The compiled rules; rebuilt when `Steer` changes.
#[derive(Resource, Default)]
struct Compiled {
    deny: Vec<(regex::Regex, String)>,
    hold: Vec<regex::Regex>,
}

/// A held call waiting for a decision, and the decisions made.
#[derive(Resource, Default, Debug)]
pub struct Approvals {
    /// Calls waiting, oldest first: (effect entity, tool name, arguments).
    pub pending: VecDeque<(Entity, String, String)>,
    pub approved: Vec<Entity>,
    pub denied: Vec<Entity>,
}

impl Approvals {
    pub fn approve_next(&mut self) {
        if let Some((entity, ..)) = self.pending.pop_front() {
            self.approved.push(entity);
        }
    }
    pub fn deny_next(&mut self) {
        if let Some((entity, ..)) = self.pending.pop_front() {
            self.denied.push(entity);
        }
    }
}

/// Deliverable retries spent, per run.
#[derive(Component, Default)]
struct DeliverableRetries(usize);

pub struct SteerPlugin;

impl Plugin for SteerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Steer>()
            .init_resource::<Compiled>()
            .init_resource::<Approvals>()
            .add_systems(
                RigSchedule,
                (
                    (compile, gate_bash, resolve_holds).chain().in_set(BusSet::Gate),
                    shape_results.in_set(BusSet::Judge),
                    demand_deliverables.in_set(RigSet::Judge),
                ),
            );
    }
}

fn compile(steer: Res<Steer>, mut compiled: ResMut<Compiled>) {
    if !steer.is_changed() {
        return;
    }
    compiled.deny = steer
        .deny
        .iter()
        .filter_map(|(pattern, reason)| regex::Regex::new(pattern).ok().map(|r| (r, reason.clone())))
        .collect();
    compiled.hold = steer.hold.iter().filter_map(|p| regex::Regex::new(p).ok()).collect();
}

fn bash_command(effect: &PendingEffect, slot: &ToolCallSlot) -> Option<String> {
    if slot.name != "bash" {
        return None;
    }
    let EffectKind::ToolCall { args, .. } = &effect.kind else { return None };
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v["command"].as_str().map(str::to_owned))
}

/// Deny or hold a fresh bash call before the bus dispatches it.
fn gate_bash(
    fresh: Query<(Entity, &PendingEffect, &ToolCallSlot), (Without<Issued>, Without<EffectOutcome>, Without<Held>)>,
    compiled: Res<Compiled>,
    steer: Res<Steer>,
    mut approvals: ResMut<Approvals>,
    mut transcript: ResMut<Transcript>,
    mut commands: Commands,
) {
    for (entity, effect, slot) in &fresh {
        let Some(command) = bash_command(effect, slot) else { continue };
        if let Some((_, reason)) = compiled.deny.iter().find(|(r, _)| r.is_match(&command)) {
            commands
                .entity(entity)
                .insert(EffectOutcome(Err(ErrorReport::new(ErrorKind::Denied, format!("denied: {reason}")))));
            transcript.push(Event::Denied { name: slot.name.clone(), reason: reason.clone() });
            continue;
        }
        if compiled.hold.iter().any(|r| r.is_match(&command)) {
            commands.entity(entity).insert(Held);
            transcript.push(Event::Held { name: slot.name.clone(), args: command.clone() });
            if steer.auto_approve {
                approvals.approved.push(entity);
            } else {
                approvals.pending.push_back((entity, slot.name.clone(), command));
            }
        }
    }
}

/// Apply the decisions: an approved hold goes on, a denied one is answered.
fn resolve_holds(mut approvals: ResMut<Approvals>, held: Query<Entity, With<Held>>, mut commands: Commands) {
    for entity in approvals.approved.drain(..) {
        if held.contains(entity) {
            commands.entity(entity).remove::<Held>();
        }
    }
    for entity in approvals.denied.drain(..) {
        if held.contains(entity) {
            commands
                .entity(entity)
                .remove::<Held>()
                .insert(EffectOutcome(Err(ErrorReport::new(ErrorKind::Denied, "denied by the reviewer"))));
        }
    }
}

/// Cut an over-long tool result to head and tail for history; the record
/// already holds the full answer.
fn shape_results(mut outcomes: Query<&mut EffectOutcome, (With<ToolCallSlot>, Added<EffectOutcome>)>, steer: Res<Steer>) {
    for mut outcome in &mut outcomes {
        let Ok(Outcome::ToolResult { result }) = &outcome.0 else { continue };
        let Some(text) = result.output().as_text() else { continue };
        if text.chars().count() <= steer.max_result_chars {
            continue;
        }
        let half = steer.max_result_chars / 2;
        let head: String = text.chars().take(half).collect();
        let tail: String = text.chars().rev().take(half).collect::<Vec<_>>().into_iter().rev().collect();
        let shaped = format!("{head}\n\n[... result cut to {} chars for history; the full output was {} chars ...]\n\n{tail}", steer.max_result_chars, text.chars().count());
        outcome.0 = Ok(Outcome::ToolResult { result: ToolResult::success(ToolOutput::text(shaped)) });
    }
}

/// A text-only answer while a required file is missing is not the end.
fn demand_deliverables(
    turns: Query<(Entity, &Outputs, &ChildOf), (With<Turn>, Without<Materialised>, Without<Retry>)>,
    mut runs: Query<Option<&mut DeliverableRetries>, With<RunOf>>,
    steer: Res<Steer>,
    mut commands: Commands,
) {
    if steer.deliverables.is_empty() {
        return;
    }
    for (turn, outs, child_of) in &turns {
        if !outs.done || outs.content.iter().any(|c| matches!(c, AssistantContent::ToolCall(_))) {
            continue;
        }
        let missing: Vec<String> = steer
            .deliverables
            .iter()
            .filter(|p| !p.exists())
            .map(|p| p.display().to_string())
            .collect();
        if missing.is_empty() {
            continue;
        }
        let run = child_of.parent();
        let spent = match runs.get_mut(run) {
            Ok(Some(mut retries)) => {
                retries.0 += 1;
                retries.0
            }
            Ok(None) => {
                commands.entity(run).insert(DeliverableRetries(1));
                1
            }
            Err(_) => continue,
        };
        if spent > steer.max_deliverable_retries {
            continue;
        }
        commands.entity(turn).insert(Retry {
            feedback: Some(format!(
                "Not finished: these required files do not exist yet: {}. Create them with the tools, verify they exist, then give the final summary.",
                missing.join(", ")
            )),
        });
    }
}
