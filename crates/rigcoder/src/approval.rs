//! Host decisions bound to one prepared tool invocation, before bus dispatch.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use bevy_ecs::prelude::*;
use bevy_tasks::{IoTaskPool, Task, block_on, poll_once};
use rig::{
    effect::{EffectKind, tool_key},
    error::{ErrorKind, ErrorReport},
    tool::{ToolContext, ToolExecutionError},
};
use rig_ecs::{
    agent::ToolCallSlot,
    bus::{EffectOutcome, Held, Issued, PendingEffect, Progress, Seq, ToolInputs},
};
use serde::{Deserialize, Serialize};

pub use crate::file_change::FilePreview;
use crate::{
    Event, Mode, Setup, Transcript, Workspace,
    file_change::{PreparedFileChange, digest},
};

/// Host approval behavior, frozen on each submitted run.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    Auto,
    Deny,
    Ask,
}

#[derive(Component, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct RunApproval(pub ApprovalMode);

/// Policy ownership of a hold. The bus's `Held` is only its dispatch barrier;
/// the runtime may independently remove that barrier to release concurrency.
#[derive(Component)]
pub struct PolicyHold;

#[derive(Component)]
pub(crate) struct BashNeedsApproval;

/// Everything the host needs to approve the exact operation.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalRequest {
    pub operation_id: String,
    pub tool: String,
    pub arguments: String,
    pub file: Option<FilePreview>,
    #[serde(skip)]
    entity: Entity,
}

impl ApprovalRequest {
    /// Complete human review text with terminal controls rendered visibly.
    /// The underlying bytes and their digests remain unchanged.
    pub fn terminal_preview(&self) -> String {
        let mut text = format!(
            "Operation {}\nTool: {}\nArguments:\n{}\n",
            self.operation_id, self.tool, self.arguments,
        );
        if let Some(file) = &self.file {
            text.push_str(&format!(
                "File: {:?}\nSource SHA-256: {}\nResult SHA-256: {}\nPrepared diff:\n{}\nExact prepared contents (controls escaped):\n{}\n",
                file.path, file.before_sha256.as_deref().unwrap_or("new file"),
                file.after_sha256, file.diff, file.after,
            ));
        }
        terminal_text(&text)
    }
}

/// Render untrusted text without executing terminal or bidirectional controls.
pub fn terminal_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if (character.is_control() && character != '\n')
            || matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

#[derive(Resource, Default, Debug)]
pub struct Approvals {
    pub pending: VecDeque<ApprovalRequest>,
    decisions: HashMap<String, bool>,
}

impl Approvals {
    /// Returns false for an obsolete or unknown request. A decision never
    /// targets whichever operation happens to occupy an old entity slot.
    pub fn decide(&mut self, operation_id: &str, approve: bool) -> bool {
        let Some(index) = self
            .pending
            .iter()
            .position(|request| request.operation_id == operation_id)
        else {
            return false;
        };
        let request = self.pending.remove(index).expect("located request");
        self.decisions.insert(request.operation_id, approve);
        true
    }

    pub fn approve_next(&mut self) {
        self.decide_next(true);
    }
    pub fn deny_next(&mut self) {
        self.decide_next(false);
    }
    fn decide_next(&mut self, approve: bool) {
        if let Some(request) = self.pending.front() {
            let id = request.operation_id.clone();
            self.decide(&id, approve);
        }
    }
}

pub(crate) enum PreparedOperation {
    File {
        change: Box<PreparedFileChange>,
        receipt: String,
    },
    Bash,
}

/// Only the product gate can construct this runtime scope. It is never
/// serialized or accepted from tool arguments, and can be consumed only once.
pub(crate) struct Permit {
    tool: String,
    arguments: serde_json::Value,
    operation: Mutex<Option<PreparedOperation>>,
    cancelled: Arc<AtomicBool>,
}

impl Permit {
    fn take(
        context: &ToolContext,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<(Arc<Self>, PreparedOperation), ToolExecutionError> {
        let permit = context.scope::<Self>().ok_or_else(|| {
            ToolExecutionError::refused("tool has no approved prepared invocation")
        })?;
        if permit.cancelled.load(Ordering::Acquire)
            || permit.tool != tool
            || permit.arguments != *args
        {
            return Err(ToolExecutionError::refused(
                "approved invocation is cancelled or its arguments changed",
            ));
        }
        let operation = permit
            .operation
            .lock()
            .map_err(|_| ToolExecutionError::refused("approval lock poisoned"))?
            .take()
            .ok_or_else(|| {
                ToolExecutionError::refused("approved invocation was already consumed")
            })?;
        Ok((permit, operation))
    }

    pub(crate) fn apply_file(
        context: &ToolContext,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<String, ToolExecutionError> {
        let (permit, operation) = Self::take(context, tool, args)?;
        let PreparedOperation::File { change, receipt } = operation else {
            return Err(ToolExecutionError::refused(
                "approved invocation is not a file change",
            ));
        };
        change
            .apply_if(|| !permit.cancelled.load(Ordering::Acquire))
            .map_err(|error| ToolExecutionError::refused(error.to_string()))?;
        Ok(receipt)
    }

    pub(crate) fn take_bash(
        context: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<Arc<Self>, ToolExecutionError> {
        let (permit, operation) = Self::take(context, "bash", args)?;
        if !matches!(operation, PreparedOperation::Bash) {
            return Err(ToolExecutionError::refused(
                "approved invocation is not a command",
            ));
        }
        Ok(permit)
    }

    pub(crate) fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }
}

struct PreparedRequest {
    operation: PreparedOperation,
    file: Option<FilePreview>,
    operation_id: String,
}

enum Phase {
    Preparing(Task<Result<PreparedRequest, String>>),
    Waiting {
        request: ApprovalRequest,
        permit: Arc<Permit>,
    },
    Approved(Arc<Permit>),
}

#[derive(Component)]
pub(crate) struct Invocation {
    tool: String,
    arguments: String,
    mode: ApprovalMode,
    phase: Phase,
}

impl Drop for Invocation {
    fn drop(&mut self) {
        if let Phase::Waiting { permit, .. } | Phase::Approved(permit) = &self.phase {
            permit.cancelled.store(true, Ordering::Release);
        }
    }
}

pub(crate) fn cancel_outcome(
    event: On<Insert, EffectOutcome>,
    invocations: Query<&Invocation>,
    mut approvals: ResMut<Approvals>,
) {
    if let Ok(invocation) = invocations.get(event.entity)
        && let Phase::Waiting { permit, .. } | Phase::Approved(permit) = &invocation.phase
    {
        permit.cancelled.store(true, Ordering::Release);
    }
    approvals
        .pending
        .retain(|request| request.entity != event.entity);
}

pub(crate) fn cancel_run(
    event: On<Insert, rig_ecs::agent::Cancelled>,
    invocations: Query<(Entity, &Invocation)>,
    parents: Query<&ChildOf>,
    mut approvals: ResMut<Approvals>,
) {
    for (entity, invocation) in &invocations {
        let mut ancestor = entity;
        while let Ok(parent) = parents.get(ancestor) {
            ancestor = parent.parent();
            if ancestor == event.entity {
                if let Phase::Waiting { permit, .. } | Phase::Approved(permit) = &invocation.phase {
                    permit.cancelled.store(true, Ordering::Release);
                }
                approvals.pending.retain(|request| request.entity != entity);
                break;
            }
        }
    }
}

fn deny(world: &mut World, entity: Entity, tool: &str, reason: String) {
    let (scope, mode) = run_identity(world, entity);
    let operation = world
        .get::<Invocation>(entity)
        .and_then(|invocation| match &invocation.phase {
            Phase::Waiting { request, .. } => Some(request.operation_id.clone()),
            Phase::Preparing(_) | Phase::Approved(_) => None,
        })
        .unwrap_or_default();
    crate::observe::emit(
        world.get_resource::<rig_ecs::bus::Witnessing>(),
        subject_of(world, entity, &scope),
        "approval",
        &crate::observe::Approval {
            operation,
            tool: tool.to_owned(),
            mode: format!("{mode:?}").to_ascii_lowercase(),
            decision: "denied".into(),
            reason: Some(reason.clone()),
        },
    );
    // The outcome first, then the hold: a hold removed from an answered
    // intent is the denial, not a release, to the witness.
    world
        .entity_mut(entity)
        .insert(EffectOutcome(Err(ErrorReport::new(
            ErrorKind::Denied,
            &reason,
        ))));
    world
        .entity_mut(entity)
        .remove::<(Held, PolicyHold, rig_ecs::bus::PolicyHeld, Invocation)>();
    world.resource_mut::<Transcript>().push(Event::Denied {
        name: tool.to_owned(),
        reason,
    });
    world.resource_mut::<Progress>().mark();
}

/// The subject of a tool-call effect entity for a host fact: the run's
/// scope and the effect's dispatch order and key.
fn subject_of(world: &World, entity: Entity, scope: &str) -> rig::observe::Subject {
    rig::observe::Subject {
        scope: Some(scope.to_owned()),
        order: world.get::<Seq>(entity).map(|seq| seq.0),
        effect: world.get::<Issued>(entity).map(|issued| issued.0),
        key: world.get::<PendingEffect>(entity).map(|e| e.key.clone()),
        family: world.get::<PendingEffect>(entity).map(|e| e.kind.family()),
        ..rig::observe::Subject::default()
    }
}

fn run_identity(world: &World, mut entity: Entity) -> (String, ApprovalMode) {
    loop {
        if let Some(rig_ecs::bus::Scope(scope)) = world.get::<rig_ecs::bus::Scope>(entity) {
            return (
                scope.clone(),
                world
                    .get::<RunApproval>(entity)
                    .map_or_else(|| *world.resource::<ApprovalMode>(), |mode| mode.0),
            );
        }
        let Some(parent) = world.get::<ChildOf>(entity) else {
            return ("unscoped".to_owned(), *world.resource::<ApprovalMode>());
        };
        entity = parent.parent();
    }
}

/// Poll preparation and host decisions without blocking the ECS schedule.
/// Runs after ordinary scope/deny gates and after the runtime's Release set.
pub(crate) fn gate(world: &mut World) {
    // Exact effect replay has no live tool handlers or file execution. Policy
    // replay and restored pending approvals need the checkpoint envelope.
    if matches!(world.resource::<Setup>().mode, Mode::Replay(_)) {
        return;
    }
    let mut decisions = std::mem::take(&mut world.resource_mut::<Approvals>().decisions);
    let mut candidates: Vec<_> = world
        .query::<(Entity, &Seq, &PendingEffect, &ToolCallSlot)>()
        .iter(world)
        .filter(|(_, _, _, slot)| matches!(slot.name.as_str(), "write_file" | "edit_file" | "bash"))
        .map(|(entity, seq, effect, slot)| (seq.0, entity, effect.clone(), slot.name.clone()))
        .collect();
    candidates.sort_by_key(|(seq, ..)| *seq);
    for (seq, entity, effect, tool) in candidates {
        if world.get::<EffectOutcome>(entity).is_some() {
            world
                .entity_mut(entity)
                .remove::<(Invocation, PolicyHold)>();
            continue;
        }
        if world.get::<Issued>(entity).is_some() {
            continue;
        }
        let EffectKind::ToolCall {
            name: kind_name,
            args,
        } = &effect.kind
        else {
            continue;
        };
        if effect.key != tool_key(&tool) || kind_name != &tool {
            deny(
                world,
                entity,
                &tool,
                "tool routing changed before approval".into(),
            );
            continue;
        }
        let mut invocation = if let Some(invocation) = world.entity_mut(entity).take::<Invocation>()
        {
            invocation
        } else {
            if world.get::<Held>(entity).is_some() {
                continue;
            }
            let (scope, mut mode) = run_identity(world, entity);
            if world.get::<BashNeedsApproval>(entity).is_some() && mode == ApprovalMode::Auto {
                mode = ApprovalMode::Ask;
            }
            if mode == ApprovalMode::Deny {
                deny(
                    world,
                    entity,
                    &tool,
                    "denied by the run's approval policy".into(),
                );
                continue;
            }
            let root = world.resource::<Workspace>().root.clone();
            let name = tool.clone();
            let arguments = args.clone();
            let identity = format!("{scope}/{seq}");
            let task = IoTaskPool::get().spawn(async move {
                let args: serde_json::Value = serde_json::from_str(&arguments)
                    .map_err(|error| format!("invalid tool arguments: {error}"))?;
                let operation = if name == "bash" {
                    if args
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .is_none()
                    {
                        return Err("missing string argument command".into());
                    }
                    PreparedOperation::Bash
                } else {
                    crate::tools::prepare_file(&root, &name, args).await?
                };
                let file = match &operation {
                    PreparedOperation::File { change, .. } => {
                        Some(change.preview().map_err(|error| error.to_string())?)
                    }
                    PreparedOperation::Bash => None,
                };
                // Display diff generation has a deadline and may choose a
                // different valid representation. Bind identity to bytes.
                let file_identity = file
                    .as_ref()
                    .map(|file| (&file.path, &file.before_sha256, &file.after_sha256));
                let operation_id = digest(
                    &serde_json::to_vec(&(&identity, &name, &arguments, file_identity))
                        .expect("serializable invocation"),
                );
                Ok(PreparedRequest {
                    operation,
                    file,
                    operation_id,
                })
            });
            world.resource_mut::<Progress>().mark();
            Invocation {
                tool: tool.clone(),
                arguments: args.clone(),
                mode,
                phase: Phase::Preparing(task),
            }
        };
        if invocation.tool != tool || invocation.arguments != *args {
            deny(
                world,
                entity,
                &tool,
                "tool arguments changed after preparation began".into(),
            );
            continue;
        }
        if let Phase::Preparing(task) = &mut invocation.phase
            && let Some(result) = block_on(poll_once(task))
        {
            let PreparedRequest {
                operation,
                file,
                operation_id,
            } = match result {
                Ok(prepared) => prepared,
                Err(error) => {
                    deny(world, entity, &tool, error);
                    continue;
                }
            };
            let request = ApprovalRequest {
                operation_id,
                tool: tool.clone(),
                arguments: args.clone(),
                file,
                entity,
            };
            let permit = Arc::new(Permit {
                tool: tool.clone(),
                arguments: serde_json::from_str(args).expect("prepared valid JSON"),
                operation: Mutex::new(Some(operation)),
                cancelled: Arc::new(AtomicBool::new(false)),
            });
            let (scope, _) = run_identity(world, entity);
            crate::observe::emit(
                world.get_resource::<rig_ecs::bus::Witnessing>(),
                subject_of(world, entity, &scope),
                "approval",
                &crate::observe::Approval {
                    operation: request.operation_id.clone(),
                    tool: tool.clone(),
                    mode: format!("{:?}", invocation.mode).to_ascii_lowercase(),
                    decision: if invocation.mode == ApprovalMode::Ask {
                        "held".into()
                    } else {
                        "prepared".into()
                    },
                    reason: None,
                },
            );
            if invocation.mode == ApprovalMode::Ask {
                world.resource_mut::<Transcript>().push(Event::Held {
                    name: tool.clone(),
                    args: serde_json::to_string(&request).expect("serializable request"),
                });
                world
                    .resource_mut::<Approvals>()
                    .pending
                    .push_back(request.clone());
            }
            invocation.phase = Phase::Waiting { request, permit };
            world.resource_mut::<Progress>().mark();
        }
        if let Phase::Waiting { request, permit } = &invocation.phase {
            let decision = if invocation.mode == ApprovalMode::Auto {
                Some(true)
            } else {
                decisions.remove(&request.operation_id)
            };
            match decision {
                Some(true) => {
                    let (scope, _) = run_identity(world, entity);
                    crate::observe::emit(
                        world.get_resource::<rig_ecs::bus::Witnessing>(),
                        subject_of(world, entity, &scope),
                        "approval",
                        &crate::observe::Approval {
                            operation: request.operation_id.clone(),
                            tool: tool.clone(),
                            mode: format!("{:?}", invocation.mode).to_ascii_lowercase(),
                            decision: "approved".into(),
                            reason: None,
                        },
                    );
                    let mut context = world
                        .get::<ToolInputs>(entity)
                        .map(|inputs| inputs.0.clone())
                        .unwrap_or_default();
                    context = context.with_scope(permit.clone());
                    world.entity_mut(entity).insert(ToolInputs(context));
                    invocation.phase = Phase::Approved(permit.clone());
                    world.resource_mut::<Progress>().mark();
                }
                Some(false) => {
                    deny(world, entity, &tool, "denied by the reviewer".into());
                    continue;
                }
                None => {}
            }
        }
        if matches!(invocation.phase, Phase::Approved(_)) {
            // The runtime lifts only the holds it placed (`BatchHeld`): this
            // gate's hold is its own to remove. A call the batch still holds
            // keeps `Held` until the batch releases it in call order.
            let batch_held = world.get::<rig_ecs::systems::BatchHeld>(entity).is_some();
            world
                .entity_mut(entity)
                .remove::<(PolicyHold, rig_ecs::bus::PolicyHeld)>();
            if !batch_held {
                world.entity_mut(entity).remove::<Held>();
            }
        } else {
            // `PolicyHeld` beside `Held`: on a call the batch also holds, the
            // batch lifts only its own marker and `Held` stands for this gate.
            world
                .entity_mut(entity)
                .insert((Held, PolicyHold, rig_ecs::bus::PolicyHeld));
        }
        world.entity_mut(entity).insert(invocation);
    }
    let active: std::collections::HashSet<_> = world
        .query::<(Entity, &Invocation)>()
        .iter(world)
        .map(|(entity, _)| entity)
        .collect();
    world
        .resource_mut::<Approvals>()
        .pending
        .retain(|request| active.contains(&request.entity));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_text_exposes_terminal_and_directional_controls_without_executing_them() {
        let bytes = "safe\u{1b}[2J\rhidden\u{202e}text\tend\n";
        let request = ApprovalRequest {
            operation_id: "operation-id".into(),
            tool: "write_file".into(),
            arguments: serde_json::json!({"content": bytes}).to_string(),
            file: Some(FilePreview {
                path: "file\u{1b}[H".into(),
                before_sha256: None,
                after_sha256: digest(bytes.as_bytes()),
                after: bytes.into(),
                diff: format!("+{bytes}"),
            }),
            entity: Entity::PLACEHOLDER,
        };
        let preview = request.terminal_preview();
        assert!(preview.starts_with("Operation operation-id\n"));
        assert!(!preview.chars().any(|c| c.is_control() && c != '\n'));
        assert!(!preview.contains('\u{202e}'));
        assert!(preview.contains("\\u{1b}[2J\\rhidden\\u{202e}text\\tend"));
        assert_eq!(request.file.unwrap().after, bytes);
    }

    fn file_permit(path: &std::path::Path) -> (Arc<Permit>, serde_json::Value) {
        let arguments = serde_json::json!({"path": path, "content": "after"});
        let change = crate::file_change::FileSource::read(path)
            .unwrap()
            .prepare(b"after".to_vec());
        let permit = Arc::new(Permit {
            tool: "write_file".into(),
            arguments: arguments.clone(),
            operation: Mutex::new(Some(PreparedOperation::File {
                change: Box::new(change),
                receipt: "written".into(),
            })),
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        (permit, arguments)
    }

    #[test]
    fn file_permits_require_matching_arguments_and_are_consumed_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "before").unwrap();
        let (permit, arguments) = file_permit(&path);
        assert!(Permit::apply_file(&ToolContext::new(), "write_file", &arguments).is_err());
        let context = ToolContext::new().with_scope(permit);
        let mut changed = arguments.clone();
        changed["content"] = "unapproved".into();
        assert!(Permit::apply_file(&context, "write_file", &changed).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
        assert_eq!(
            Permit::apply_file(&context, "write_file", &arguments).unwrap(),
            "written"
        );
        std::fs::write(&path, "later").unwrap();
        assert!(Permit::apply_file(&context, "write_file", &arguments).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "later");
    }

    #[test]
    fn session_cancel_revokes_even_an_issued_file_permit_before_execution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "before").unwrap();
        let (permit, arguments) = file_permit(&path);
        let context = ToolContext::new().with_scope(permit.clone());
        let mut world = World::new();
        rig_ecs::bus::install_bus(&mut world, rig::serve::ServingPolicy::default());
        world.init_resource::<Approvals>();
        world.init_resource::<crate::Conversation>();
        world.add_observer(cancel_run);
        let run = world.spawn_empty().id();
        world.resource_mut::<crate::Conversation>().active = Some(run);
        let turn = world.spawn(ChildOf(run)).id();
        let effect = world
            .spawn((
                ChildOf(turn),
                Issued(rig::effect::EffectId::from_raw(42)),
                Invocation {
                    tool: "write_file".into(),
                    arguments: arguments.to_string(),
                    mode: ApprovalMode::Ask,
                    phase: Phase::Approved(permit),
                },
            ))
            .id();
        crate::cancel(&mut world, "cancel before the issued callback executes");
        assert!(world.get::<Issued>(effect).is_some());
        assert!(Permit::apply_file(&context, "write_file", &arguments).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "before");
    }
}
