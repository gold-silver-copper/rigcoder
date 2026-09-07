//! Steering systems: what a hook would be, as systems on the bus's sets.
//! `BusSet::Gate` sees a tool call before dispatch (deny, hold, patch);
//! `RigSet::Judge` sees an outcome before history reads it (rewrite). The
//! `systems` lane of the improvement loop edits this file; the rules live
//! in the [`Steer`] resource so settings can tune them without new code.

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

/// The tunable rules. Empty until PR D of the harness programme fills in
/// the deny and hold lists, result shaping and the deliverables check.
#[derive(Resource, Debug, Clone, Default)]
pub struct Steer {}

pub struct SteerPlugin;

impl Plugin for SteerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Steer>();
    }
}
