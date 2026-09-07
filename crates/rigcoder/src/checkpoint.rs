//! Checkpoints: the run graph as a scene after every turn, and the
//! workspace as a tarball beside it, so a run can be resumed from a turn in
//! a fresh world with the workspace as it was.
//!
//! Only between turns: an in-flight stream cannot be saved (rig-ecs refuses
//! an unfinished delivered prefix), and a turn's tool batch is out only
//! while the turn is unmaterialised.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use bevy_ecs::prelude::*;
use rig_ecs::{
    agent::{
        Failed, Run, RunOf, RunSeq, Settled, Turn,
        scene::{SceneKind, WorldScene, load_world, save_world},
    },
    systems::Materialised,
};

use crate::{
    Workspace,
    session::{AgentHandle, Conversation, Event, Transcript},
};

/// Where to write checkpoints; `None` disables them.
#[derive(Resource, Debug, Clone, Default)]
pub struct Checkpoint {
    pub dir: Option<PathBuf>,
    /// Also tar the workspace beside each scene.
    pub tar: bool,
    pub turns_saved: usize,
}

pub fn scene_path(dir: &std::path::Path, turn: usize) -> PathBuf {
    dir.join(format!("turn-{turn:03}.scene.json"))
}

pub fn tar_path(dir: &std::path::Path, turn: usize) -> PathBuf {
    dir.join(format!("turn-{turn:03}.tar"))
}

/// Turns materialised so far; the observer only counts.
#[derive(Resource, Debug, Default)]
pub struct MaterialisedTurns(pub usize);

pub fn count_materialised(
    added: On<Add, Materialised>,
    turns: Query<(), With<Turn>>,
    mut count: ResMut<MaterialisedTurns>,
) {
    if turns.get(added.event().entity).is_ok() {
        count.0 += 1;
    }
}

/// After `Materialise` and before `Settle`: the turn is complete and the
/// next has not been advanced, so the graph is between turns. Saves once
/// per materialised turn.
pub fn save_between_turns(world: &mut World) {
    let Some(dir) = world.resource::<Checkpoint>().dir.clone() else {
        return;
    };
    let materialised = world.get_resource::<MaterialisedTurns>().map_or(0, |m| m.0);
    if materialised <= world.resource::<Checkpoint>().turns_saved {
        return;
    }
    // A turn whose tool batch is still out is not between turns: a scene
    // saved now would re-issue the unanswered calls on load. Wait for the
    // pass in which the batch lands (nothing pending, nothing in flight).
    let pending = world
        .query_filtered::<(), (
            With<rig_ecs::bus::PendingEffect>,
            Without<rig_ecs::bus::EffectOutcome>,
        )>()
        .iter(world)
        .count();
    if pending > 0 {
        return;
    }
    let tar = world.resource::<Checkpoint>().tar;
    match write_checkpoint(world, &dir, materialised, tar) {
        Ok(()) => world.resource_mut::<Checkpoint>().turns_saved = materialised,
        Err(error) => {
            // One actionable failure, rather than another failure every tick.
            world.resource_mut::<Checkpoint>().dir = None;
            world
                .resource_mut::<Transcript>()
                .push(Event::Failed(format!("checkpoint {materialised}: {error}")));
        }
    }
}

fn write_checkpoint(world: &mut World, dir: &Path, turn: usize, tar: bool) -> Result<(), String> {
    let scene = save_world(world).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec(&scene).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let dir = dir.canonicalize().map_err(|e| e.to_string())?;
    if scene_path(&dir, turn).exists() || tar_path(&dir, turn).exists() {
        return Err("checkpoint already exists; use a fresh checkpoint directory".to_owned());
    }
    let scene_tmp = dir.join(format!("turn-{turn:03}.scene.json.partial"));
    let tar_tmp = dir.join(format!("turn-{turn:03}.tar.partial"));
    let result = (|| {
        if tar {
            let root = world
                .resource::<Workspace>()
                .root
                .canonicalize()
                .map_err(|e| e.to_string())?;
            if dir.starts_with(&root) {
                return Err("--checkpoint-tar requires a checkpoint directory outside the workspace, so archives cannot include themselves".to_owned());
            }
            let output = Command::new("tar")
                .arg("-cf")
                .arg(&tar_tmp)
                .arg("-C")
                .arg(&root)
                .arg(".")
                .output()
                .map_err(|e| format!("could not run tar: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "tar failed ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        std::fs::write(&scene_tmp, json).map_err(|e| e.to_string())?;
        if tar {
            std::fs::rename(&tar_tmp, tar_path(&dir, turn)).map_err(|e| e.to_string())?;
        }
        // Publish the scene last: a scene is available only after its paired
        // archive was successfully written.
        std::fs::rename(&scene_tmp, scene_path(&dir, turn)).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(scene_tmp);
        let _ = std::fs::remove_file(tar_tmp);
    }
    result
}

/// Load a saved scene into a world whose model and tools are bound, and
/// make its run the active one. The workspace must already be as it was.
pub fn resume(world: &mut World, scene: &WorldScene) -> Result<Entity, rig::error::ErrorReport> {
    if world.resource::<Conversation>().active.is_some() {
        return Err(rig::error::ErrorReport::new(
            rig::error::ErrorKind::Request,
            "cannot resume while another run is active",
        ));
    }
    // Reject an ambiguous/empty conversation before loading any effect or
    // graph entity. The caller may keep ticking the app after this error.
    let saved_runs: Vec<_> = scene
        .graph
        .entities
        .iter()
        .filter(|entity| entity.kind == SceneKind::Run)
        .collect();
    if saved_runs.is_empty() {
        return Err(rig::error::ErrorReport::new(
            rig::error::ErrorKind::Request,
            "the scene holds no run",
        ));
    }
    if saved_runs
        .iter()
        .filter(|entity| {
            !entity.components.contains_key("settled") && !entity.components.contains_key("failed")
        })
        .count()
        > 1
    {
        return Err(rig::error::ErrorReport::new(
            rig::error::ErrorKind::Request,
            "the scene holds more than one unfinished run",
        ));
    }
    let loaded = load_world(scene, world)?;
    let runs: Vec<Entity> = loaded
        .graph
        .iter()
        .copied()
        .filter(|e| world.get::<Run>(*e).is_some())
        .collect();
    let active: Vec<Entity> = runs
        .iter()
        .copied()
        .filter(|e| world.get::<Settled>(*e).is_none() && world.get::<Failed>(*e).is_none())
        .collect();
    if active.len() > 1 {
        return Err(rig::error::ErrorReport::new(
            rig::error::ErrorKind::Request,
            "the scene holds more than one unfinished run",
        ));
    }
    let run = active
        .first()
        .copied()
        .or_else(|| {
            runs.iter()
                .copied()
                .max_by_key(|e| world.get::<RunSeq>(*e).map_or(0, |seq| seq.0))
        })
        .ok_or_else(|| {
            rig::error::ErrorReport::new(rig::error::ErrorKind::Request, "the scene holds no run")
        })?;
    let sequence = runs
        .iter()
        .filter_map(|entity| world.get::<rig_ecs::bus::Scope>(*entity))
        .filter_map(|scope| scope.0.strip_prefix("rigcoder/run/")?.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    {
        let mut conversation = world.resource_mut::<Conversation>();
        conversation.active = Some(run);
        conversation.runs = conversation.runs.saturating_add(runs.len()).max(sequence);
    }
    if let Some(agent) = world.get::<RunOf>(run).map(|run_of| run_of.0) {
        world.resource_mut::<AgentHandle>().agent = agent;
    }
    // Load observers can see partially restored entities; recover this count
    // from the finished graph instead of relying on insertion order.
    let materialised = world
        .query_filtered::<(), (With<Turn>, With<Materialised>)>()
        .iter(world)
        .count();
    world.resource_mut::<MaterialisedTurns>().0 = materialised;
    world.resource_mut::<Checkpoint>().turns_saved = materialised;
    world.resource_mut::<Transcript>().push(Event::User {
        text: "(resumed from a checkpoint)".to_owned(),
    });
    // Terminal components were inserted before Conversation was rebound.
    // Notify our observers only now, with the complete restored graph.
    if let Some(failed) = world.entity_mut(run).take::<Failed>() {
        world.entity_mut(run).insert(failed);
    } else if let Some(settled) = world.entity_mut(run).take::<Settled>() {
        world.entity_mut(run).insert(settled);
    }
    Ok(run)
}
