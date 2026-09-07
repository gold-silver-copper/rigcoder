//! Checkpoints: the run graph as a scene after every turn, and the
//! workspace as a tarball beside it, so a run can be resumed from a turn in
//! a fresh world with the workspace as it was.
//!
//! Only between turns: an in-flight stream cannot be saved (rig-ecs refuses
//! an unfinished delivered prefix), and a turn's tool batch is out only
//! while the turn is unmaterialised.

use std::{path::PathBuf, process::Command};

use bevy_ecs::prelude::*;
use rig_ecs::{
    agent::{Turn, scene::{WorldScene, load_world, save_world}},
    systems::Materialised,
};

use crate::{Workspace, session::{Conversation, Event, Transcript}};

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

pub fn count_materialised(added: On<Add, Materialised>, turns: Query<(), With<Turn>>, mut count: ResMut<MaterialisedTurns>) {
    if turns.get(added.event().entity).is_ok() {
        count.0 += 1;
    }
}

/// After `Materialise` and before `Settle`: the turn is complete and the
/// next has not been advanced, so the graph is between turns. Saves once
/// per materialised turn.
pub fn save_between_turns(world: &mut World) {
    let Some(dir) = world.resource::<Checkpoint>().dir.clone() else { return };
    let materialised = world.get_resource::<MaterialisedTurns>().map_or(0, |m| m.0);
    if materialised <= world.resource::<Checkpoint>().turns_saved {
        return;
    }
    // A turn whose tool batch is still out is not between turns: a scene
    // saved now would re-issue the unanswered calls on load. Wait for the
    // pass in which the batch lands (nothing pending, nothing in flight).
    let pending = world
        .query_filtered::<(), (With<rig_ecs::bus::PendingEffect>, Without<rig_ecs::bus::EffectOutcome>)>()
        .iter(world)
        .count();
    if pending > 0 {
        return;
    }
    let tar = world.resource::<Checkpoint>().tar;
    let turn = {
        let mut checkpoint = world.resource_mut::<Checkpoint>();
        checkpoint.turns_saved = materialised;
        materialised
    };
    let _ = std::fs::create_dir_all(&dir);
    match save_world(world) {
        Ok(scene) => match serde_json::to_string(&scene) {
            Ok(json) => {
                if let Err(error) = std::fs::write(scene_path(&dir, turn), json) {
                    world.resource_mut::<Transcript>().push(Event::Failed(format!("checkpoint {turn}: {error}")));
                }
            }
            Err(error) => world.resource_mut::<Transcript>().push(Event::Failed(format!("checkpoint {turn}: {error}"))),
        },
        Err(report) => world.resource_mut::<Transcript>().push(Event::Failed(format!("checkpoint {turn}: {report}"))),
    }
    if tar {
        let root = world.resource::<Workspace>().root.clone();
        let _ = Command::new("tar").arg("-cf").arg(tar_path(&dir, turn)).arg("-C").arg(&root).arg(".").status();
    }
}

/// Load a saved scene into a world whose model and tools are bound, and
/// make its run the active one. The workspace must already be as it was.
pub fn resume(world: &mut World, scene: &WorldScene) -> Result<Entity, rig::error::ErrorReport> {
    let loaded = load_world(scene, world)?;
    let run = loaded
        .graph
        .iter()
        .copied()
        .find(|e| world.get::<rig_ecs::agent::Run>(*e).is_some())
        .ok_or_else(|| rig::error::ErrorReport::new(rig::error::ErrorKind::Request, "the scene holds no run"))?;
    let mut conversation = world.resource_mut::<Conversation>();
    conversation.active = Some(run);
    conversation.runs += 1;
    world.resource_mut::<Transcript>().push(Event::User { text: "(resumed from a checkpoint)".to_owned() });
    Ok(run)
}
