//! Structured product failures, independent of rendered error text.

use rig::{
    error::{ErrorKind, ErrorReport},
    observe::{
        Action, AdapterEnding, AdapterErrorEnvelope, AdapterEvent, AdapterVerdict,
        ObservationTrace, Subject,
    },
};
use serde::{Deserialize, Serialize};

/// Observed boundary, not a causal judgment about the agent or candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureBoundary {
    /// A provider response reported the error.
    ProviderResponse,
    /// The HTTP transport failed without a response status.
    Transport,
    /// Response decoding or validation failed.
    Decode,
    /// A host or runtime operation failed.
    Host,
    /// The available facts do not establish a boundary.
    Unknown,
}

/// Provider evidence associated with the completion that failed the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureAttempt {
    /// Dispatch subject, including execution-local scope and effect links.
    pub subject: Subject,
    /// Logical completion identity within this execution.
    pub operation: String,
    /// HTTP send ordinal within the operation.
    pub attempt: u64,
    /// Host retry ordinal when supplied by the caller.
    pub host_attempt: Option<std::num::NonZeroU64>,
    /// Actual response status, distinct from an in-band error's status.
    pub response_status: Option<u16>,
    /// Adapter closure, independent of the product's decision to stop.
    pub ending: Option<AdapterEnding>,
    /// Original provider verdict, separate from host retry policy.
    pub verdict: AdapterVerdict,
    /// Original scrubbed in-band error metadata, when present.
    pub error_envelope: Option<AdapterErrorEnvelope>,
}

/// A bounded product failure. Missing provider facts are explicitly absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureDetail {
    /// Stable classification, copied from ErrorKind when a report exists.
    pub kind: String,
    /// Runtime failure variant or host action that produced this failure.
    pub origin: String,
    /// Diagnostic category, never a causal blame assignment.
    pub boundary: FailureBoundary,
    /// Bounded, scrubbed human-readable detail; consumers classify by kind.
    pub message: String,
    /// Provider/report retryability, not the host's decision to retry.
    pub retryable: Option<bool>,
    /// Status retained by the error report; an in-band error can differ from HTTP.
    pub http_status: Option<u16>,
    /// Unambiguously associated provider attempt, if observable.
    pub adapter: Option<Box<FailureAttempt>>,
}

impl FailureDetail {
    /// Describe a host failure with an application-owned stable kind code.
    pub fn host(kind: &str, message: &str, secrets: &[String]) -> Self {
        Self {
            kind: kind.into(),
            origin: kind.into(),
            boundary: FailureBoundary::Host,
            message: rig::observe::scrub_diagnostic(message, secrets),
            retryable: Some(false),
            http_status: None,
            adapter: None,
        }
    }

    /// Preserve typed report classification and retryability, scrubbing detail.
    pub fn report(origin: &str, report: &ErrorReport, secrets: &[String]) -> Self {
        let boundary = if origin != "provider" {
            FailureBoundary::Host
        } else {
            match report.kind {
                ErrorKind::Http { status: Some(_) } | ErrorKind::ProviderResponse => {
                    FailureBoundary::ProviderResponse
                }
                // Statusless HTTP reports also cover request construction and
                // response content-type rejection, not just transport errors.
                ErrorKind::Http { status: None } => FailureBoundary::Unknown,
                ErrorKind::Response => FailureBoundary::Decode,
                // JSON errors also occur during request serialization, so a
                // report alone cannot attribute them to response decoding.
                // The stream transport also converts HTTP failures into the
                // generic Provider kind; adapter evidence can refine it later.
                ErrorKind::Json | ErrorKind::Provider | ErrorKind::Other => {
                    FailureBoundary::Unknown
                }
                _ => FailureBoundary::Host,
            }
        };
        Self {
            kind: report.kind.code().into(),
            origin: origin.into(),
            boundary,
            message: rig::observe::scrub_diagnostic(&report.message, secrets),
            retryable: Some(report.is_retryable()),
            http_status: report.http_status,
            adapter: None,
        }
    }

    /// Project a runtime failure without parsing its Display or Debug output.
    pub fn runtime(failure: Option<&rig_ecs::agent::Failure>, secrets: &[String]) -> Self {
        use rig_ecs::agent::Failure;
        match failure {
            Some(Failure::Provider(report)) => Self::report("provider", report, secrets),
            Some(Failure::Cancelled(report)) => Self::report("cancelled", report, secrets),
            Some(Failure::Tool(report)) => Self::report("tool", report, secrets),
            Some(Failure::Memory(report)) => Self::report("memory", report, secrets),
            Some(Failure::MaxTurns { limit }) => Self::host(
                "max_turns",
                &format!("model-call limit {limit} reached"),
                secrets,
            ),
            Some(Failure::UnknownToolCall { name }) => Self::host(
                "unknown_tool_call",
                &format!("unknown tool: {name}"),
                secrets,
            ),
            Some(Failure::OutputToolCollision { name }) => Self::host(
                "output_tool_collision",
                &format!("output tool conflicts with granted tool: {name}"),
                secrets,
            ),
            Some(Failure::Unsupported(message)) => Self::host("unsupported", message, secrets),
            None => Self {
                kind: "unknown".into(),
                origin: "unknown".into(),
                boundary: FailureBoundary::Unknown,
                message: "failure detail unavailable".into(),
                retryable: None,
                http_status: None,
                adapter: None,
            },
        }
    }

    /// Attach evidence only for the failed completion in this run scope.
    /// A missing landing, missing adapter facts, or multiple candidate sends
    /// leaves identity unknown. Event arrival order does not establish which
    /// concurrent send caused a handler's error.
    pub fn attach(&mut self, scope: &str, trace: &ObservationTrace) {
        self.adapter = None;
        if self.origin != "provider" || trace.dropped != 0 {
            return;
        }
        let effects: std::collections::BTreeSet<_> = trace
            .observations
            .iter()
            .filter_map(|o| {
                if o.subject.scope.as_deref() != Some(scope)
                    || o.subject.family != Some(rig::effect::EffectFamily::Completion)
                {
                    return None;
                }
                matches!(
                    &o.action,
                    Action::Landed {
                        outcome: rig::observe::OutcomeSummary::Err { .. }
                    } | Action::Replaced {
                        consumed: rig::observe::OutcomeSummary::Err { .. },
                        ..
                    } | Action::Denied { .. }
                )
                .then_some(o.subject.effect)
                .flatten()
            })
            .collect();
        if effects.len() != 1 {
            return;
        }
        let effect = *effects.first().expect("exactly one failed effect");
        if trace.observations.iter().any(|o| {
            o.subject.scope.as_deref() == Some(scope)
                && o.subject.effect == Some(effect)
                && matches!(
                    &o.action,
                    Action::Replaced {
                        consumed: rig::observe::OutcomeSummary::Err { .. },
                        ..
                    } | Action::Denied { .. }
                )
        }) {
            // The host's denial or replacement owns the consumed failure.
            // A successful provider attempt is still present in the trace,
            // but is not evidence that the provider produced this error.
            self.boundary = FailureBoundary::Host;
            return;
        }
        let candidates: std::collections::BTreeSet<_> = trace
            .observations
            .iter()
            .filter(|o| {
                o.subject.scope.as_deref() == Some(scope) && o.subject.effect == Some(effect)
            })
            .filter_map(|o| match &o.action {
                Action::Adapter { observation } => observation
                    .attempt
                    .map(|attempt| (observation.operation.as_str(), attempt)),
                _ => None,
            })
            .collect();
        if candidates.len() != 1 {
            return;
        }
        for o in &trace.observations {
            if o.subject.scope.as_deref() != Some(scope) || o.subject.effect != Some(effect) {
                continue;
            }
            let Action::Adapter { observation: fact } = &o.action else {
                continue;
            };
            let Some(attempt) = fact.attempt else {
                continue;
            };
            if self
                .adapter
                .as_ref()
                .is_none_or(|a| a.operation != fact.operation || a.attempt != attempt)
            {
                self.adapter = Some(Box::new(FailureAttempt {
                    subject: o.subject.clone(),
                    operation: fact.operation.clone(),
                    attempt,
                    host_attempt: fact.host_attempt,
                    response_status: None,
                    ending: None,
                    verdict: AdapterVerdict::default(),
                    error_envelope: None,
                }));
            }
            let Some(adapter) = &mut self.adapter else {
                continue;
            };
            match &fact.event {
                AdapterEvent::Response { status } => adapter.response_status = Some(*status),
                AdapterEvent::Finished { ending } => {
                    adapter.ending = Some(ending.clone());
                    if let AdapterEnding::Error { boundary, .. } = ending {
                        use rig::observe::AdapterErrorBoundary as B;
                        self.boundary = match boundary {
                            B::Request => FailureBoundary::Host,
                            B::ProviderResponse => FailureBoundary::ProviderResponse,
                            B::Decode => FailureBoundary::Decode,
                            B::Transport => FailureBoundary::Transport,
                            B::Unknown => FailureBoundary::Unknown,
                        };
                    }
                }
                AdapterEvent::Provider { verdict } => {
                    if verdict.finish_reason.is_some() {
                        adapter
                            .verdict
                            .finish_reason
                            .clone_from(&verdict.finish_reason);
                    }
                    if verdict.block_reason.is_some() {
                        adapter
                            .verdict
                            .block_reason
                            .clone_from(&verdict.block_reason);
                    }
                    if verdict.detail.is_some() {
                        adapter.verdict.detail.clone_from(&verdict.detail);
                    }
                    if verdict.model.is_some() {
                        adapter.verdict.model.clone_from(&verdict.model);
                    }
                }
                AdapterEvent::ErrorEnvelope { error } => {
                    adapter.error_envelope = Some(error.clone())
                }
                AdapterEvent::Corrupt { .. } if self.kind == "json" => {
                    self.boundary = FailureBoundary::Decode
                }
                _ => {}
            }
        }
    }

    /// Whether typed provenance identifies a replay failure for the CLI exit code.
    pub fn is_replay_failure(&self) -> bool {
        self.kind == "divergence"
            || matches!(
                self.origin.as_str(),
                "replay_setup" | "replay_binding" | "replay_validation"
            )
    }
}

impl std::fmt::Display for FailureDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl rig::observe::HostAction for FailureDetail {
    const KIND: &'static str = "rigcoder/failure";
}

/// Record a terminal host error in the transcript and optional witness.
/// Callers decide whether an error is terminal; a rejected, recoverable API
/// operation must not end the session merely because it returned an error.
/// Provider credentials from the configured connection are scrubbed before emission.
pub fn record_host_report(world: &mut bevy_ecs::world::World, origin: &str, report: &ErrorReport) {
    let secrets = crate::model::world_diagnostic_secrets(world);
    record_world(
        world,
        Subject::default(),
        origin,
        FailureDetail::report(origin, report, &secrets),
    );
}

/// Record one product failure in both the transcript and the optional witness.
/// Runtime failures that will retry emit their fact separately, without a
/// terminal transcript event.
pub(crate) fn record(
    transcript: &mut crate::Transcript,
    witness: Option<&rig_ecs::bus::Witnessing>,
    subject: Subject,
    policy: &str,
    reason: FailureDetail,
) {
    crate::observe::emit(witness, subject, policy, &reason);
    transcript.push(crate::Event::Failed { reason });
}

/// Exclusive-world counterpart for submission, cancellation and checkpoints.
pub(crate) fn record_world(
    world: &mut bevy_ecs::world::World,
    subject: Subject,
    policy: &str,
    reason: FailureDetail,
) {
    let witness = world.get_resource::<rig_ecs::bus::Witnessing>().cloned();
    record(
        &mut world.resource_mut::<crate::Transcript>(),
        witness.as_ref(),
        subject,
        policy,
        reason,
    );
}

#[cfg(test)]
mod tests;
