//! Scrub diagnostic fields on exported copies, without rewriting replay inputs.

use rig::{
    error::ErrorReport,
    observe::{Action, ObservationTrace, OutcomeSummary, Reason, scrub_diagnostic},
};

fn text(value: &mut String, secrets: &[String]) {
    *value = scrub_diagnostic(value, secrets);
}

fn optional(value: &mut Option<String>, secrets: &[String]) {
    if let Some(value) = value {
        text(value, secrets);
    }
}

fn report(error: &mut ErrorReport, secrets: &[String]) {
    text(&mut error.message, secrets);
    optional(&mut error.code, secrets);
    optional(&mut error.request_id, secrets);
    for source in &mut error.source_chain {
        text(source, secrets);
    }
    if let Some(response) = &mut error.provider_response {
        text(&mut response.body, secrets);
        optional(&mut response.provider_request_id, secrets);
        // These headers are already excluded from ErrorReport's wire form.
        // Do not expose their raw in-memory copy through the product export API.
        response.headers = None;
    }
}

pub(crate) fn effect_log(log: &mut crate::EffectLog, secrets: &[String]) {
    for record in &mut log.records {
        if let Err(error) = &mut record.outcome {
            report(error, secrets);
        }
    }
    for errors in log.header.stream_errors.values_mut() {
        for error in errors {
            report(&mut error.error, secrets);
        }
    }
}

pub(crate) fn scene(
    scene: &mut rig_ecs::agent::scene::WorldScene,
    secrets: &[String],
) -> Result<(), serde_json::Error> {
    use rig_ecs::agent::{Failed, Failure};
    for effect in &mut scene.effects.effects {
        if let Some(Err(error)) = &mut effect.outcome {
            report(error, secrets);
        }
        if let Some(stream) = &mut effect.streamed {
            if let Some(Err(error)) = &mut stream.outcome {
                report(error, secrets);
            }
            for (_, error) in &mut stream.errors {
                report(error, secrets);
            }
        }
    }
    for entity in &mut scene.graph.entities {
        if let Some(value) = entity.components.get_mut("failed") {
            // Decode only the library-owned failure component. A malformed
            // component rejects publication instead of persisting it unsanitized.
            let mut failed: Failed = serde_json::from_value(value.clone())?;
            match &mut failed.0 {
                Failure::Provider(error)
                | Failure::Cancelled(error)
                | Failure::Tool(error)
                | Failure::Memory(error) => report(error, secrets),
                Failure::Unsupported(detail) => text(detail, secrets),
                Failure::UnknownToolCall { name } | Failure::OutputToolCollision { name } => {
                    text(name, secrets);
                }
                Failure::MaxTurns { .. } => {}
            }
            *value = serde_json::to_value(failed)?;
        }
    }
    Ok(())
}

fn reason(reason: &mut Reason, secrets: &[String]) {
    optional(&mut reason.detail, secrets);
}

fn outcome(outcome: &mut OutcomeSummary, secrets: &[String]) {
    if let OutcomeSummary::Err { reason: detail, .. } = outcome {
        reason(detail, secrets);
    }
}

pub(crate) fn observations(trace: &mut ObservationTrace, secrets: &[String]) {
    for observation in &mut trace.observations {
        match &mut observation.action {
            Action::Held { reason: detail }
            | Action::Denied { reason: detail }
            | Action::Deferred { reason: detail }
            | Action::Refused { reason: detail }
            | Action::Cancelled { reason: detail }
            | Action::CancelRequested { reason: detail }
            | Action::InvalidCall {
                resolution: detail, ..
            }
            | Action::Ended { ending: detail } => reason(detail, secrets),
            Action::Approved {
                reason: Some(detail),
            } => reason(detail, secrets),
            Action::Landed { outcome: value } => outcome(value, secrets),
            Action::Replaced { recorded, consumed } => {
                outcome(recorded, secrets);
                outcome(consumed, secrets);
            }
            Action::StreamTruncated { errors, .. } => {
                for error in errors {
                    reason(error, secrets);
                }
            }
            // Adapter and typed product failure payloads are scrubbed at emission.
            // Request patches, feedback and streamed content are program data.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
