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
                    (compile, gate_scope, gate_bash, resolve_holds).chain().in_set(BusSet::Gate),
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

/// Where the agent may write. Empty `allow` means anywhere not denied. A
/// `write_file`, `edit_file` or `bash` call naming a path outside `allow`
/// or inside `deny` is denied with a reason the model reads, before
/// dispatch: the improvement loop's lane is enforced here, not by a
/// `git checkout` afterwards.
#[derive(Resource, Debug, Clone, Default)]
pub struct Scope {
    pub root: PathBuf,
    pub allow: Vec<PathBuf>,
    pub deny: Vec<PathBuf>,
}

impl Scope {
    fn normalise(&self, path: &str) -> PathBuf {
        let p = std::path::Path::new(path);
        if p.is_absolute() { p.to_path_buf() } else { self.root.join(p) }
    }

    fn under(path: &std::path::Path, prefix: &std::path::Path) -> bool {
        path.starts_with(prefix)
    }

    /// The first path that breaks the scope, with the rule it broke. The
    /// most specific rule wins: an allowed file inside a denied directory
    /// is allowed.
    pub fn violation<'a>(&self, paths: impl Iterator<Item = &'a str>) -> Option<(String, String)> {
        if self.allow.is_empty() && self.deny.is_empty() {
            return None;
        }
        for raw in paths {
            let path = self.normalise(raw);
            // An ancestor of an allowed path (a `cd` or an `ls` on the way) is fine.
            if self.allow.iter().any(|a| a != &path && Self::under(a, &path)) {
                continue;
            }
            let deepest_allow = self.allow.iter().filter(|a| Self::under(&path, a)).map(|a| a.components().count()).max();
            let deepest_deny = self.deny.iter().filter(|d| Self::under(&path, d)).map(|d| d.components().count()).max();
            if let Some(deny) = deepest_deny
                && deepest_allow.is_none_or(|allow| allow < deny)
            {
                let denied = self.deny.iter().filter(|d| Self::under(&path, d)).max_by_key(|d| d.components().count()).expect("matched");
                return Some((raw.to_owned(), format!("under the denied path {}", denied.display())));
            }
            if deepest_allow.is_some() {
                continue;
            }
            // A directory above an allowed file (a `cd` or an `ls`) is fine.
            if !self.allow.is_empty() && !self.allow.iter().any(|a| Self::under(a, &path)) {
                return Some((raw.to_owned(), "outside the allowed paths".to_owned()));
            }
        }
        None
    }

    /// Paths a bash command might write: tokens with a `/` in them, and
    /// dotted tokens that name an existing file under `root` (so `os.path`
    /// or `1.0` are not paths), plus redirection targets. Conservative on
    /// purpose: a denial is a tool result the model can act on.
    pub fn bash_paths_in(root: &std::path::Path, command: &str) -> Vec<String> {
        command
            .split(|c: char| c.is_whitespace() || c == ';' || c == '|' || c == '&' || c == '(' || c == ')' || c == '\'' || c == '"' || c == '`')
            .map(|t| t.trim_start_matches(['>', '<']))
            .filter(|t| !t.is_empty() && !t.starts_with('-') && !t.starts_with('$') && !t.contains("://"))
            .filter(|t| t.contains('/') || (t.contains('.') && !t.ends_with('.') && root.join(t).is_file()))
            .map(str::to_owned)
            .collect()
    }

    pub fn bash_paths(command: &str) -> Vec<String> {
        Self::bash_paths_in(std::path::Path::new("/nonexistent"), command)
    }

    /// Can this command change files? Reads (`cat`, `ls`, `grep`, a `cargo
    /// check`) are not the scope's business; redirections and the usual
    /// writing commands are.
    pub fn bash_writes(command: &str) -> bool {
        if command.contains('>') {
            return true;
        }
        let writers = ["tee", "rm", "mv", "cp", "touch", "mkdir", "rmdir", "truncate", "dd", "install", "ln", "patch", "chmod", "chown", "sed -i", "sed -E -i", "perl -pi", "perl -i", "perl -0pi", "git checkout", "git reset", "git clean", "git stash", "git apply", "git rm", "git mv", "git commit", "git push", "cargo fmt", "cargo fix", "rustfmt"];
        let padded = format!(" {command} ");
        writers.iter().any(|w| padded.contains(&format!(" {w} ")) || padded.contains(&format!(";{w} ")) || padded.contains(&format!("&{w} ")) || padded.contains(&format!("|{w} ")) || padded.contains(&format!("({w} ")))
    }
}

/// Deny a file write or a bash command whose paths break the scope.
pub fn gate_scope(
    fresh: Query<(Entity, &PendingEffect, &ToolCallSlot), (Without<Issued>, Without<EffectOutcome>, Without<Held>)>,
    scope: Option<Res<Scope>>,
    mut transcript: ResMut<Transcript>,
    mut commands: Commands,
) {
    let Some(scope) = scope else { return };
    for (entity, effect, slot) in &fresh {
        let EffectKind::ToolCall { args, .. } = &effect.kind else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else { continue };
        let paths: Vec<String> = match slot.name.as_str() {
            "write_file" | "edit_file" => value["path"].as_str().map(|p| vec![p.to_owned()]).unwrap_or_default(),
            "bash" => value["command"].as_str().filter(|c| Scope::bash_writes(c)).map(|c| Scope::bash_paths_in(&scope.root, c)).unwrap_or_default(),
            _ => continue,
        };
        if let Some((path, why)) = scope.violation(paths.iter().map(String::as_str)) {
            let reason = format!("{path} is {why}; this run may only change: {}", scope.allow.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "));
            commands
                .entity(entity)
                .insert(EffectOutcome(Err(ErrorReport::new(ErrorKind::Denied, format!("denied: {reason}")))));
            transcript.push(Event::Denied { name: slot.name.clone(), reason });
        }
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            root: PathBuf::from("/repo"),
            allow: vec![PathBuf::from("/repo/crates/rigcoder/src/prompt.md"), PathBuf::from("/repo/crates/rigcoder/src/tools/")],
            deny: vec![PathBuf::from("/repo/harness/"), PathBuf::from("/repo/Cargo.toml")],
        }
    }

    #[test]
    fn allowed_denied_and_outside() {
        let s = scope();
        assert!(s.violation(["crates/rigcoder/src/prompt.md"].into_iter()).is_none());
        assert!(s.violation(["crates/rigcoder/src/tools/bash.rs"].into_iter()).is_none());
        assert_eq!(s.violation(["harness/iterate.rs"].into_iter()).unwrap().1, "under the denied path /repo/harness/");
        assert_eq!(s.violation(["crates/rigcoder/src/lib.rs"].into_iter()).unwrap().1, "outside the allowed paths");
        assert!(s.violation(["/repo/Cargo.toml"].into_iter()).is_some());
    }

    #[test]
    fn an_allowed_file_inside_a_denied_directory_is_allowed() {
        let s = Scope {
            root: PathBuf::from("/repo"),
            allow: vec![PathBuf::from("/repo/harness/notes/gen-001-prompt.md"), PathBuf::from("/repo/crates/rigcoder/src/prompt.md")],
            deny: vec![PathBuf::from("/repo/harness/")],
        };
        assert!(s.violation(["harness/notes/gen-001-prompt.md"].into_iter()).is_none());
        assert!(s.violation(["harness/notes"].into_iter()).is_none(), "a directory above an allowed file");
        assert!(s.violation(["harness/ledger.jsonl"].into_iter()).is_some());
        assert!(Scope::bash_paths("python3 -c \"import os; os.path.join(1.0, 2)\"").is_empty());
    }

    #[test]
    fn bash_paths_are_found_conservatively() {
        let paths = Scope::bash_paths("cargo check --workspace && echo x > harness/ledger.jsonl; sed -i s/a/b/ crates/rigcoder/src/prompt.md");
        assert!(paths.contains(&"harness/ledger.jsonl".to_owned()), "{paths:?}");
        assert!(paths.contains(&"crates/rigcoder/src/prompt.md".to_owned()), "{paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with("--")), "{paths:?}");
        let s = scope();
        assert!(s.violation(paths.iter().map(String::as_str)).is_some());
        assert!(s.violation(Scope::bash_paths("cargo check --workspace").iter().map(String::as_str)).is_none());
    }

    #[test]
    fn only_writing_commands_are_scoped() {
        assert!(!Scope::bash_writes("cat harness/ledger.jsonl | head"));
        assert!(!Scope::bash_writes("ls ~/.cargo/git/checkouts && grep -rn foo crates/"));
        assert!(!Scope::bash_writes("cargo check --workspace"));
        assert!(Scope::bash_writes("echo x > harness/ledger.jsonl"));
        assert!(Scope::bash_writes("sed -i s/a/b/ crates/rigcoder/src/prompt.md"));
        assert!(Scope::bash_writes("cd x && rm -rf harness"));
        assert!(Scope::bash_writes("git checkout -- harness/ledger.jsonl"));
    }
}
