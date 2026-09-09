//! What rigcoder's own policies decide, as typed facts for the world's
//! witness (`rig_ecs::bus::Witnessing`): approvals, steering denials,
//! result shaping and provider retries. The library observes the bus and
//! the run (holds, denials, replacements, endings, truncations); these name
//! the policy that made each decision and carry what only it knows.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use rig::observe::{Action, Emitter, HostAction, ObservationLog, ObservationTrace, Stage, Subject};
use rig_ecs::bus::Witnessing;
use serde::{Deserialize, Serialize};

/// The witness's sink, kept so a host reads the trace back.
#[derive(Resource, Clone)]
pub struct Observations(pub Arc<ObservationLog>);

/// Environment in which an observation's measurements were taken.
/// This describes execution, not the provenance of a recording being replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Live,
    CassetteReplay,
    PacedReplay,
    LocalOnly,
    EffectLogReplay,
}

/// The host's clock implementation, independently of execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockSource {
    HostMonotonic,
    Scripted,
    Absent,
}

/// Host-owned provenance for measurements in the current observation capture.
#[derive(Resource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasurementContext {
    pub execution_mode: ExecutionMode,
    pub clock_source: ClockSource,
}

/// Origin of provider content, independently of replay mode and pacing.
/// An absent value means unknown, not that no recording was used.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingProvenance {
    Live,
    Derived,
    NotApplicable,
}

/// Existing trace format with host metadata; Rig readers can still read the
/// flattened trace. Missing context in older artifacts remains unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationArtifact {
    #[serde(flatten)]
    pub trace: ObservationTrace,
    #[serde(default)]
    pub measurement_context: Option<MeasurementContext>,
    #[serde(default)]
    pub recording_provenance: Option<RecordingProvenance>,
}

struct HostClock(std::time::Instant);

impl rig::observe::Clock for HostClock {
    fn elapsed(&self) -> std::time::Duration {
        self.0.elapsed()
    }
}

/// Install a fresh host-clock capture before submitting any work. The caller
/// identifies the actual execution mode, never the recording's origin.
pub fn install_monotonic(world: &mut World, execution_mode: ExecutionMode) {
    let observations = Arc::new(
        ObservationLog::default().with_clock(Arc::new(HostClock(std::time::Instant::now()))),
    );
    Witnessing::install(world, observations.clone());
    world.insert_resource(Observations(observations));
    world.insert_resource(MeasurementContext {
        execution_mode,
        clock_source: ClockSource::HostMonotonic,
    });
}

/// Export the scrubbed trace and its separately labelled measurement context.
pub fn artifact(world: &World) -> Option<ObservationArtifact> {
    Some(ObservationArtifact {
        trace: trace(world)?,
        measurement_context: world.get_resource::<MeasurementContext>().cloned(),
        recording_provenance: world.get_resource::<RecordingProvenance>().copied(),
    })
}

/// The observations so far, with bounded, scrubbed diagnostic reasons.
pub fn trace(world: &World) -> Option<ObservationTrace> {
    let secrets = crate::model::world_diagnostic_secrets(world);
    world.get_resource::<Observations>().map(|observations| {
        let mut trace = observations.0.trace();
        crate::artifacts::observations(&mut trace, &secrets);
        trace
    })
}

/// The session finished normally.
pub fn finalize(world: &World) {
    if let Some(observations) = world.get_resource::<Observations>() {
        observations.0.finalize();
    }
}

/// rigcoder as an emitter: the policy module and the crate version.
pub fn emitter(policy: &str) -> Emitter {
    Emitter::versioned(format!("rigcoder/{policy}"), env!("CARGO_PKG_VERSION"))
}

/// Emit one host fact, if a witness is installed and the fact serializes.
pub fn emit(witness: Option<&Witnessing>, subject: Subject, policy: &str, fact: &impl HostAction) {
    let Some(witness) = witness else {
        return;
    };
    match fact.action() {
        Ok(action) => witness.emit(subject, Stage::Host, emitter(policy), action),
        Err(error) => tracing::warn!("a host observation did not serialize: {error}"),
    }
}

/// A raw action under rigcoder's name (for facts already shaped by the
/// library's vocabulary, such as a replacement).
pub fn emit_action(witness: Option<&Witnessing>, subject: Subject, policy: &str, action: Action) {
    if let Some(witness) = witness {
        witness.emit(subject, Stage::Host, emitter(policy), action);
    }
}

/// The approval gate's decision on one prepared invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// The prepared operation's identity (bound to its arguments and bytes).
    pub operation: String,
    /// The tool.
    pub tool: String,
    /// The run's approval mode when the decision was made.
    pub mode: String,
    /// `prepared`, `held`, `approved`, `denied`.
    pub decision: String,
    /// Why, for a denial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HostAction for Approval {
    const KIND: &'static str = "rigcoder/approval";
}

/// A steering rule refused a bash command or a scoped file write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerDenial {
    /// `bash` or the file tool.
    pub tool: String,
    /// The rule family: `deny`, `invalid_rule`, `scope`.
    pub rule: String,
    /// The reason the model reads.
    pub reason: String,
}

impl HostAction for SteerDenial {
    const KIND: &'static str = "rigcoder/steer";
}

/// A tool result was cut to head and tail for history; the record keeps
/// the full answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultShaped {
    pub tool: String,
    pub chars: usize,
    pub kept: usize,
}

impl HostAction for ResultShaped {
    const KIND: &'static str = "rigcoder/result_shaped";
}

/// The session decided to submit the prompt again after a transient
/// provider failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRetry {
    /// Initial completion's execution-local identity, absent without observation context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    pub attempt: usize,
    pub wait_secs: u64,
    pub reason: String,
}

impl HostAction for ProviderRetry {
    const KIND: &'static str = "rigcoder/provider_retry";
}
