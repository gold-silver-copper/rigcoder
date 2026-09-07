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
    tool::ToolOutput,
};
use rig_ecs::{
    agent::{Outputs, Retry, RunOf, ToolCallSlot, Turn},
    bus::{BusSet, EffectOutcome, Held, Issued, PendingEffect, RigSchedule},
    systems::{Materialised, RigSet},
};

use crate::session::{Event, Transcript};

/// The tunable rules.
#[derive(Resource, Debug, Clone, serde::Serialize)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::system::RunSystemOnce;
    use rig::tool::{ToolExecutionError, ToolResult};

    #[test]
    fn shaping_preserves_failure_and_refusal_dispositions() {
        for error in [
            ToolExecutionError::timeout("x".repeat(200)),
            ToolExecutionError::refused("x".repeat(200)),
        ] {
            let refused = error.is_refusal();
            let mut world = World::new();
            world.insert_resource(Steer {
                max_result_chars: 20,
                ..Default::default()
            });
            let entity = world
                .spawn((
                    ToolCallSlot {
                        index: 0,
                        id: rig::message::ToolCallId::new("test").unwrap(),
                        provider: None,
                        name: "bash".to_owned(),
                    },
                    EffectOutcome(Ok(Outcome::ToolResult {
                        result: ToolResult::failed(error),
                    })),
                ))
                .id();
            world.run_system_once(shape_results).unwrap();
            let Ok(Outcome::ToolResult { result }) = &world.get::<EffectOutcome>(entity).unwrap().0
            else {
                panic!("expected tool result")
            };
            assert_eq!(result.is_refused(), refused);
            assert_eq!(result.is_error(), !refused);
            assert!(result.output().render().contains("result cut"));
        }
    }

    #[test]
    fn canceled_holds_leave_the_approval_queue_and_keep_their_outcome() {
        let mut world = World::new();
        let entity = world
            .spawn((
                Held,
                EffectOutcome(Err(ErrorReport::new(ErrorKind::Cancelled, "cancelled"))),
            ))
            .id();
        world.insert_resource(Approvals {
            pending: VecDeque::from([(entity, "bash".to_owned(), "touch file".to_owned())]),
            denied: vec![entity],
            ..Default::default()
        });
        world.run_system_once(resolve_holds).unwrap();
        assert!(world.resource::<Approvals>().pending.is_empty());
        assert_eq!(
            world
                .get::<EffectOutcome>(entity)
                .unwrap()
                .0
                .as_ref()
                .unwrap_err()
                .kind,
            ErrorKind::Cancelled
        );
    }
}

impl Default for Steer {
    fn default() -> Self {
        Self {
            deny: vec![
                (
                    r"(^|[;&|]\s*)(find|grep|rg|ls)\s+(-\S+\s+)*/(\s|$)".to_owned(),
                    "searching the whole filesystem hangs the run; search inside the workspace"
                        .to_owned(),
                ),
                (
                    r"(^|[;&|]\s*)grep\s+-[a-zA-Z]*r[a-zA-Z]*\s+.*\s/(\s|$)".to_owned(),
                    "a recursive grep of / hangs the run; search inside the workspace".to_owned(),
                ),
                (
                    r"rm\s+(-\S+\s+)*-?[a-zA-Z]*[rR][a-zA-Z]*\s+/(\s|$)".to_owned(),
                    "refusing to delete /".to_owned(),
                ),
                (
                    r"(^|[;&|]\s*)(shutdown|reboot|halt|poweroff)(\s|$)".to_owned(),
                    "no".to_owned(),
                ),
            ],
            hold: Vec::new(),
            auto_approve: true,
            max_result_chars: 30_000,
            deliverables: Vec::new(),
            max_deliverable_retries: 2,
        }
    }
}

impl Steer {
    pub fn validate(&self) -> Result<(), regex::Error> {
        for pattern in self
            .deny
            .iter()
            .map(|(pattern, _)| pattern)
            .chain(self.hold.iter())
        {
            regex::Regex::new(pattern)?;
        }
        Ok(())
    }
}

/// The compiled rules; rebuilt when `Steer` changes.
#[derive(Resource, Default)]
struct Compiled {
    deny: Vec<(regex::Regex, String)>,
    hold: Vec<regex::Regex>,
    invalid: Option<String>,
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

/// Deliverable retries spent, per run (saved with a scene).
#[derive(Component, Default, serde::Serialize, serde::Deserialize)]
pub struct DeliverableRetries(pub usize);

pub struct SteerPlugin;

impl Plugin for SteerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Steer>()
            .init_resource::<Compiled>()
            .init_resource::<Approvals>()
            .add_systems(
                RigSchedule,
                (
                    (compile, gate_scope, gate_bash, resolve_holds)
                        .chain()
                        .in_set(BusSet::Gate),
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
    compiled.invalid = steer
        .validate()
        .err()
        .map(|error| format!("invalid steering rule: {error}"));
    compiled.deny = steer
        .deny
        .iter()
        .filter_map(|(pattern, reason)| {
            regex::Regex::new(pattern).ok().map(|r| (r, reason.clone()))
        })
        .collect();
    compiled.hold = steer
        .hold
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect();
}

fn bash_command(effect: &PendingEffect, slot: &ToolCallSlot) -> Option<String> {
    if slot.name != "bash" {
        return None;
    }
    let EffectKind::ToolCall { args, .. } = &effect.kind else {
        return None;
    };
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v["command"].as_str().map(str::to_owned))
}

/// Deny or hold a fresh bash call before the bus dispatches it.
type Ungated = (Without<Issued>, Without<EffectOutcome>, Without<Held>);

fn gate_bash(
    fresh: Query<(Entity, &PendingEffect, &ToolCallSlot), Ungated>,
    compiled: Res<Compiled>,
    steer: Res<Steer>,
    mut approvals: ResMut<Approvals>,
    mut transcript: ResMut<Transcript>,
    mut commands: Commands,
) {
    for (entity, effect, slot) in &fresh {
        let Some(command) = bash_command(effect, slot) else {
            continue;
        };
        if let Some(reason) = &compiled.invalid {
            commands
                .entity(entity)
                .insert(EffectOutcome(Err(ErrorReport::new(
                    ErrorKind::Denied,
                    reason,
                ))));
            transcript.push(Event::Denied {
                name: slot.name.clone(),
                reason: reason.clone(),
            });
            continue;
        }
        if let Some((_, reason)) = compiled.deny.iter().find(|(r, _)| r.is_match(&command)) {
            commands
                .entity(entity)
                .insert(EffectOutcome(Err(ErrorReport::new(
                    ErrorKind::Denied,
                    format!("denied: {reason}"),
                ))));
            transcript.push(Event::Denied {
                name: slot.name.clone(),
                reason: reason.clone(),
            });
            continue;
        }
        if compiled.hold.iter().any(|r| r.is_match(&command)) {
            commands.entity(entity).insert(Held);
            transcript.push(Event::Held {
                name: slot.name.clone(),
                args: command.clone(),
            });
            if steer.auto_approve {
                approvals.approved.push(entity);
            } else {
                approvals
                    .pending
                    .push_back((entity, slot.name.clone(), command));
            }
        }
    }
}

/// Apply the decisions: an approved hold goes on, a denied one is answered.
fn resolve_holds(
    mut approvals: ResMut<Approvals>,
    held: Query<Entity, (With<Held>, Without<EffectOutcome>)>,
    mut commands: Commands,
) {
    // Cancellation resolves held effects too. Do not let an old approval
    // overwrite that outcome or stay at the front of the UI's queue.
    approvals
        .pending
        .retain(|(entity, ..)| held.contains(*entity));
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
                .insert(EffectOutcome(Err(ErrorReport::new(
                    ErrorKind::Denied,
                    "denied by the reviewer",
                ))));
        }
    }
}

/// Cut an over-long tool result to head and tail for history; the record
/// already holds the full answer.
fn shape_results(
    mut outcomes: Query<&mut EffectOutcome, (With<ToolCallSlot>, Added<EffectOutcome>)>,
    steer: Res<Steer>,
) {
    for mut outcome in &mut outcomes {
        let Ok(Outcome::ToolResult { result }) = &outcome.0 else {
            continue;
        };
        let Some(text) = result.output().as_text() else {
            continue;
        };
        if text.chars().count() <= steer.max_result_chars {
            continue;
        }
        let half = steer.max_result_chars / 2;
        let head: String = text.chars().take(half).collect();
        let tail: String = text
            .chars()
            .rev()
            .take(half)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let shaped = format!(
            "{head}\n\n[... result cut to {} chars for history; the full output was {} chars ...]\n\n{tail}",
            steer.max_result_chars,
            text.chars().count()
        );
        outcome.0 = Ok(Outcome::ToolResult {
            result: result.clone().with_output(ToolOutput::text(shaped)),
        });
    }
}

/// A text-only answer while a required file is missing is not the end.
type Judgable = (With<Turn>, Without<Materialised>, Without<Retry>);

fn demand_deliverables(
    turns: Query<(Entity, &Outputs, &ChildOf), Judgable>,
    mut runs: Query<Option<&mut DeliverableRetries>, With<RunOf>>,
    steer: Res<Steer>,
    mut commands: Commands,
) {
    if steer.deliverables.is_empty() {
        return;
    }
    for (turn, outs, child_of) in &turns {
        if !outs.done
            || outs
                .content
                .iter()
                .any(|c| matches!(c, AssistantContent::ToolCall(_)))
        {
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

/// File-tool write scope. Rules name canonical filesystem paths; the most
/// specific rule wins, with denial winning ties. Bash is disabled whenever
/// scope rules are configured because shell commands cannot be scoped by
/// inspecting their text. This is not an operating-system sandbox.
#[derive(Resource, Debug, Clone, Default, serde::Serialize)]
pub struct Scope {
    pub root: PathBuf,
    pub allow: Vec<PathBuf>,
    pub deny: Vec<PathBuf>,
}

impl Scope {
    pub fn enabled(&self) -> bool {
        !self.allow.is_empty() || !self.deny.is_empty()
    }

    /// Resolve each existing component before interpreting a later `..`.
    /// Missing leaves are allowed for new files; dangling symlinks and other
    /// filesystem errors fail closed.
    fn canonical(&self, raw: &std::path::Path) -> std::io::Result<PathBuf> {
        use std::path::Component;
        let absolute = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };
        if !absolute.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "scope root must be absolute",
            ));
        }
        let mut resolved = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(name) => {
                    resolved.push(name);
                    match std::fs::symlink_metadata(&resolved) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            resolved = resolved.canonicalize()?
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        Ok(resolved)
    }

    pub fn violation<'a>(&self, paths: impl Iterator<Item = &'a str>) -> Option<(String, String)> {
        if !self.enabled() {
            return None;
        }
        let rules = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|p| self.canonical(p))
                .collect::<std::io::Result<Vec<_>>>()
        };
        let (allow, deny) = match (rules(&self.allow), rules(&self.deny)) {
            (Ok(allow), Ok(deny)) => (allow, deny),
            (Err(error), _) | (_, Err(error)) => {
                return Some(("scope".to_owned(), format!("invalid scope rule: {error}")));
            }
        };
        for raw in paths {
            let path = match self.canonical(std::path::Path::new(raw)) {
                Ok(path) => path,
                Err(error) => {
                    return Some((raw.to_owned(), format!("cannot resolve path: {error}")));
                }
            };
            let deepest_allow = allow
                .iter()
                .filter(|a| path.starts_with(a))
                .map(|a| a.components().count())
                .max();
            let deepest_deny = deny
                .iter()
                .filter(|d| path.starts_with(d))
                .map(|d| d.components().count())
                .max();
            if deepest_deny
                .is_some_and(|denied| deepest_allow.is_none_or(|allowed| allowed <= denied))
            {
                return Some((raw.to_owned(), "under a denied path".to_owned()));
            }
            if !allow.is_empty() && deepest_allow.is_none() {
                return Some((raw.to_owned(), "outside the allowed paths".to_owned()));
            }
            // An allowed name must not mutate a second, unscoped hard link.
            #[cfg(unix)]
            if let Ok(metadata) = std::fs::metadata(&path) {
                use std::os::unix::fs::MetadataExt;
                if metadata.is_file() && metadata.nlink() > 1 {
                    return Some((
                        raw.to_owned(),
                        "a hard-linked file; scoped writes require a single link".to_owned(),
                    ));
                }
            }
        }
        None
    }
}

/// Guard file writes and disable arbitrary command execution for scoped runs.
pub fn gate_scope(
    mut fresh: Query<(Entity, &mut PendingEffect, &ToolCallSlot), Ungated>,
    scope: Option<Res<Scope>>,
    mut transcript: ResMut<Transcript>,
    mut commands: Commands,
) {
    let Some(scope) = scope.filter(|scope| scope.enabled()) else {
        return;
    };
    for (entity, mut effect, slot) in &mut fresh {
        let mut canonical_args = None;
        let reason = if slot.name == "bash" {
            Some("bash is disabled for scoped runs; use read_file, list_files, grep, write_file and edit_file. The harness runs verification after the edit".to_owned())
        } else if matches!(slot.name.as_str(), "write_file" | "edit_file") {
            let EffectKind::ToolCall { args, .. } = &effect.kind else {
                continue;
            };
            let value = serde_json::from_str::<serde_json::Value>(args).ok();
            match value {
                Some(mut value) if value["path"].is_string() => {
                    let path = value["path"].as_str().expect("checked");
                    if let Some((path, why)) = scope.violation(std::iter::once(path)) {
                        Some(format!("{path} is {why}"))
                    } else {
                        match scope.canonical(std::path::Path::new(path)) {
                            Ok(path) => match path.to_str() {
                                Some(path) => {
                                    value["path"] = serde_json::Value::String(path.to_owned());
                                    canonical_args =
                                        Some(serde_json::to_string(&value).expect("parsed JSON"));
                                    None
                                }
                                None => Some("scoped file paths must be valid UTF-8".to_owned()),
                            },
                            Err(error) => Some(format!("cannot resolve scoped file path: {error}")),
                        }
                    }
                }
                None => Some("a scoped file write requires a valid path".to_owned()),
                Some(_) => Some("a scoped file write requires a valid path".to_owned()),
            }
        } else {
            None
        };
        if let Some(canonical_args) = canonical_args
            && let EffectKind::ToolCall { args, .. } = &mut effect.kind
        {
            *args = canonical_args;
        }
        if let Some(reason) = reason {
            commands
                .entity(entity)
                .insert(EffectOutcome(Err(ErrorReport::new(
                    ErrorKind::Denied,
                    format!("denied: {reason}"),
                ))));
            transcript.push(Event::Denied {
                name: slot.name.clone(),
                reason,
            });
        }
    }
}
