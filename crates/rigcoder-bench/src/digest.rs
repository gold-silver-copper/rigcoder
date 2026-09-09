//! The failure digest: what the trials did, counted the way an engineer
//! would count it, with failed and passed trials side by side.
//!
//! Reads every `<trial>/agent/transcript.jsonl` under a job directory. The
//! reward comes from `<trial>/verifier/reward.txt`, or from `result.json`
//! when the verifier wrote none, so both this runner's layout and older
//! job directories digest.

use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod metadata;
use metadata::{EvaluationMetadata, TaskSource};

/// Counted facts about one trial.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TrialFacts {
    pub trial: String,
    pub task: Option<String>,
    /// Whether task identity was recorded or inferred from a legacy directory.
    #[serde(default)]
    pub task_source: Option<TaskSource>,
    /// Recorded trial ordinal; never inferred from the directory name.
    #[serde(default)]
    pub attempt: Option<usize>,
    /// Recorded task score. Missing, malformed or non-finite scores stay unknown.
    pub reward: Option<f64>,
    // Preserve legacy diagnostic routing for non-finite verifier values without
    // exporting them as recorded scores. Missing/unparseable values use zero.
    #[serde(skip)]
    unavailable_selection_reward: f64,
    pub tool_calls: u64,
    /// Per tool name: (ok, error) results.
    pub by_tool: BTreeMap<String, (u64, u64)>,
    /// Bash commands the tool killed at their timeout.
    pub timeouts: u64,
    /// Tool results cut in the middle for length.
    pub truncations: u64,
    /// A call identical (name and arguments) to one of the previous five.
    pub repeated_calls: u64,
    /// Results naming a path that did not exist.
    pub missing_paths: u64,
    /// The run ended with assistant text that reads as deliberation (a
    /// question, or "let me", "should I", "next I will") rather than a
    /// summary: the model meant to continue and settled instead.
    pub ended_deliberating: bool,
    /// The run ended without `settled` (failed, cancelled, or cut off).
    pub no_settle: bool,
    /// Tool calls before the first write_file or edit_file (u64::MAX: none).
    pub calls_before_first_edit: u64,
    /// How the run ended, from the transcript's `failed` line (`settled`
    /// when it settled; `unknown` when the transcript ends without either).
    #[serde(default)]
    pub ending: String,
    /// Structured transcript failure, including failures before a run starts.
    #[serde(default)]
    pub failure: Option<rigcoder::failure::FailureDetail>,
    /// What the witness saw (`observations.json`), when the trial has one.
    #[serde(default)]
    pub observed: Observed,
    /// The last tool called before the end.
    pub last_tool: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub final_text_chars: u64,
}

impl TrialFacts {
    /// Preserve existing investigation-bucket policy, including its handling of
    /// non-finite input. This is not a recorded score; export `reward` as evidence.
    fn selection_reward(&self) -> f64 {
        self.reward.unwrap_or(self.unavailable_selection_reward)
    }
}

/// Mean of each counted fact over a set of trials.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Aggregate {
    pub trials: usize,
    pub tool_calls: f64,
    pub timeouts: f64,
    pub truncations: f64,
    pub repeated_calls: f64,
    pub missing_paths: f64,
    pub ended_deliberating: f64,
    pub no_settle: f64,
    pub calls_before_first_edit: f64,
    pub input_tokens: f64,
    pub by_tool: BTreeMap<String, f64>,
    pub last_tool: BTreeMap<String, u64>,
}

/// An adapter fact with its original effect/run correlation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedAdapter {
    pub subject: rig::observe::Subject,
    pub fact: rig::observe::AdapterObservation,
}

/// One send's latest usage and closure, without summing cumulative snapshots.
/// A closure describes the provider boundary, never independent task success.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedAttempt {
    pub subject: rig::observe::Subject,
    pub operation: String,
    pub attempt: u64,
    pub host_attempt: Option<std::num::NonZeroU64>,
    pub started: bool,
    pub status: Option<u16>,
    pub ending: Option<rig::observe::AdapterEnding>,
    /// Observed body EOF, even when the adapter also produced a terminal.
    pub transport_eof: Option<ObservedEof>,
    /// Last reported value of each provider metadata field.
    pub verdict: rig::observe::AdapterVerdict,
    pub error_envelope: Option<rig::observe::AdapterErrorEnvelope>,
    /// Volatile diagnostics remain separate from semantic provider facts.
    pub analysis: rig::observe::AdapterAnalysis,
    /// Last reported snapshot, potentially partial even when the send closed.
    /// None means no usage report was observed, not zero consumption.
    pub usage: Option<rig::observe::AdapterUsage>,
}

/// Body completeness is independent of the provider verdict and task score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedEof {
    pub after: usize,
    pub partial_bytes: usize,
}

/// An actual host retry decision, independent of provider retryability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedRetry {
    pub subject: rig::observe::Subject,
    pub operation: Option<String>,
    /// One-based retry decision ordinal (retry 1 follows host attempt 1).
    pub retry: Option<u64>,
    pub wait_secs: Option<u64>,
}

/// One owner's acquisition or release, in trace order. These are transitions,
/// not an active-owner snapshot: denial, cancellation or missing facts can end
/// a hold without an ordinary release.
/// Pre-dispatch subjects join by scope and order; effect IDs may be absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedHold {
    pub subject: rig::observe::Subject,
    pub owner: rig::observe::Emitter,
    pub acquired: bool,
}

/// The decision trace, counted: the facts a failure classification needs
/// that no transcript line carries.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Observed {
    /// Execution and clock provenance; absent in legacy/unlabelled artifacts.
    #[serde(default)]
    pub measurement_context: Option<rigcoder::observe::MeasurementContext>,
    /// Provider content origin, independent of measurement execution mode.
    #[serde(default)]
    pub recording_provenance: Option<rigcoder::observe::RecordingProvenance>,
    /// Handler intervals attached to their landing/cancellation facts.
    #[serde(default)]
    pub handler_timings: Vec<ObservedHandlerTiming>,
    /// Typed provider boundary facts; no reconstruction from cassette bodies.
    #[serde(default)]
    pub adapter: Vec<ObservedAdapter>,
    /// Attempt summaries in first-observed order; failures keep their own usage.
    #[serde(default)]
    pub attempts: Vec<ObservedAttempt>,
    /// A valid trace was present, finalized, and had no dropped facts.
    pub complete: bool,
    /// Whether a trace could be decoded; missing/malformed is not empty success.
    #[serde(default)]
    pub trace_present: bool,
    /// Whether the producer explicitly finalized the trace, when available.
    #[serde(default)]
    pub finalized: Option<bool>,
    /// Facts the sink dropped; unknown when no valid trace was available.
    #[serde(default)]
    pub dropped: Option<u64>,
    /// Provider streams that ended before their terminal record.
    pub stream_truncations: u64,
    /// Tool calls a gate or steering rule denied.
    pub denials: u64,
    /// Whole-prompt provider retries the session made.
    pub provider_retries: u64,
    #[serde(default)]
    pub retry_decisions: Vec<ObservedRetry>,
    /// Intents the driver refused (no handler, re-entrant, ids exhausted).
    pub refusals: u64,
    /// Approval holds, and how many of them were denied by the reviewer.
    pub holds: u64,
    pub held_denied: u64,
    /// Named batch/policy transitions, separate from approval decision counts.
    #[serde(default)]
    pub hold_transitions: Vec<ObservedHold>,
    /// Dispatches cancelled while a handler served them (a tool still
    /// running when the run was cancelled).
    pub cancelled_in_flight: u64,
    /// Every run ending the witness saw, in order, with its scope and optional
    /// interval (`settled`, `provider`, `cancelled`, `max_turns`, …).
    pub endings: Vec<ObservedRunEnding>,
    /// Latest structured failure, retained even when a later retry settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<rigcoder::failure::FailureDetail>,
}

/// A run ending and its optional injected-clock interval, joined by scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedRunEnding {
    pub subject: rig::observe::Subject,
    pub ending: String,
    pub timing: Option<rig::observe::RunTiming>,
}

/// One measured handler boundary with its execution-local subject and outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedHandlerTiming {
    pub subject: rig::observe::Subject,
    pub timing: rig::observe::HandlerTiming,
    pub ending: rig::observe::Action,
}

/// Count what an observation trace says. A missing or unparsable trace is
/// an incomplete, empty `Observed`, never a claim that nothing happened.
pub fn observed(trace: &str) -> Observed {
    let Ok(artifact) = serde_json::from_str::<rigcoder::observe::ObservationArtifact>(trace) else {
        return Observed::default();
    };
    let trace = artifact.trace;
    let mut o = Observed {
        measurement_context: artifact.measurement_context,
        recording_provenance: artifact.recording_provenance,
        complete: trace.finalized && trace.is_complete(),
        trace_present: true,
        finalized: Some(trace.finalized),
        dropped: Some(trace.dropped),
        ..Observed::default()
    };
    let mut attempts = BTreeMap::new();
    for observation in &trace.observations {
        if let Some(timing) = &observation.handler_timing {
            o.handler_timings.push(ObservedHandlerTiming {
                subject: observation.subject.clone(),
                timing: timing.clone(),
                ending: observation.action.clone(),
            });
        }
        match &observation.action {
            rig::observe::Action::Held { .. } | rig::observe::Action::Released => {
                o.hold_transitions.push(ObservedHold {
                    subject: observation.subject.clone(),
                    owner: observation.emitter.clone(),
                    acquired: matches!(observation.action, rig::observe::Action::Held { .. }),
                });
            }
            rig::observe::Action::Adapter { observation: fact } => {
                o.adapter.push(ObservedAdapter {
                    subject: observation.subject.clone(),
                    fact: fact.clone(),
                });
                if let Some(attempt) = fact.attempt {
                    let key = (
                        observation.subject.scope.clone(),
                        fact.operation.clone(),
                        attempt,
                    );
                    let index = *attempts.entry(key).or_insert_with(|| {
                        let index = o.attempts.len();
                        o.attempts.push(ObservedAttempt {
                            subject: observation.subject.clone(),
                            operation: fact.operation.clone(),
                            attempt,
                            host_attempt: fact.host_attempt,
                            started: false,
                            status: None,
                            ending: None,
                            transport_eof: None,
                            verdict: rig::observe::AdapterVerdict::default(),
                            error_envelope: None,
                            analysis: rig::observe::AdapterAnalysis::default(),
                            usage: None,
                        });
                        index
                    });
                    let summary = &mut o.attempts[index];
                    if let Some(analysis) = &fact.analysis {
                        if analysis.response_id.is_some() {
                            summary
                                .analysis
                                .response_id
                                .clone_from(&analysis.response_id);
                        }
                        if analysis.headers.is_some() {
                            summary.analysis.headers.clone_from(&analysis.headers);
                        }
                        if analysis.timing.is_some() {
                            summary.analysis.timing.clone_from(&analysis.timing);
                        }
                    }
                    match &fact.event {
                        rig::observe::AdapterEvent::Provider { verdict } => {
                            if verdict.finish_reason.is_some() {
                                summary
                                    .verdict
                                    .finish_reason
                                    .clone_from(&verdict.finish_reason);
                            }
                            if verdict.block_reason.is_some() {
                                summary
                                    .verdict
                                    .block_reason
                                    .clone_from(&verdict.block_reason);
                            }
                            if verdict.detail.is_some() {
                                summary.verdict.detail.clone_from(&verdict.detail);
                            }
                            if verdict.model.is_some() {
                                summary.verdict.model.clone_from(&verdict.model);
                            }
                        }
                        rig::observe::AdapterEvent::ErrorEnvelope { error } => {
                            summary.error_envelope = Some(error.clone())
                        }
                        rig::observe::AdapterEvent::Started { .. } => summary.started = true,
                        rig::observe::AdapterEvent::Response { status } => {
                            summary.status = Some(*status)
                        }
                        rig::observe::AdapterEvent::Finished { ending } => {
                            summary.ending = Some(ending.clone())
                        }
                        rig::observe::AdapterEvent::Usage { usage } => {
                            summary.usage = Some(usage.clone())
                        }
                        rig::observe::AdapterEvent::TransportEof {
                            after,
                            partial_bytes,
                        } => {
                            summary.transport_eof = Some(ObservedEof {
                                after: *after,
                                partial_bytes: *partial_bytes,
                            });
                        }
                        _ => {}
                    }
                }
            }
            rig::observe::Action::StreamTruncated { .. } => o.stream_truncations += 1,
            rig::observe::Action::Denied { .. } => o.denials += 1,
            rig::observe::Action::Refused { .. } => o.refusals += 1,
            rig::observe::Action::Cancelled { .. }
                if observation.stage == rig::observe::Stage::Collect =>
            {
                o.cancelled_in_flight += 1;
            }
            rig::observe::Action::Ended { ending } => {
                o.endings.push(ObservedRunEnding {
                    subject: observation.subject.clone(),
                    ending: ending.code.clone(),
                    timing: observation.run_timing.clone(),
                });
            }
            rig::observe::Action::Host { kind, payload } => match kind.as_str() {
                "rigcoder/failure" => {
                    o.last_failure = serde_json::from_value(payload.clone()).ok();
                }
                "rigcoder/provider_retry" => {
                    o.provider_retries += 1;
                    o.retry_decisions.push(ObservedRetry {
                        subject: observation.subject.clone(),
                        operation: payload["operation"].as_str().map(str::to_owned),
                        retry: payload["attempt"].as_u64(),
                        wait_secs: payload["wait_secs"].as_u64(),
                    });
                }
                "rigcoder/approval" => match payload["decision"].as_str() {
                    Some("held") => o.holds += 1,
                    Some("denied") => o.held_denied += 1,
                    _ => {}
                },
                _ => {}
            },
            _ => {}
        }
    }
    o
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Digest {
    #[serde(default)]
    pub evaluation: Option<EvaluationMetadata>,
    pub failed: Aggregate,
    pub passed: Aggregate,
    pub trials: Vec<TrialFacts>,
}

#[derive(Deserialize)]
struct Event {
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: String,
    #[serde(default)]
    output: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    text: String,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reason: serde_json::Value,
}

const DELIBERATION: &[&str] = &[
    "let me ",
    "i'll ",
    "i will ",
    "next, i",
    "next i",
    "should i",
    "i should ",
    "now i",
    "i need to ",
    "i'm going to ",
    "let's ",
    "first, i",
    "wait",
    "what if",
    "hmm",
    "which one",
    "or should",
    "let's compare",
    "argument for",
];

/// The facts of one transcript.
pub fn facts(transcript: &str) -> TrialFacts {
    let mut f = TrialFacts::default();
    let mut recent: VecDeque<(String, String)> = VecDeque::new();
    let mut last_kind = String::new();
    let mut final_text = String::new();
    let mut seen_edit = false;
    f.calls_before_first_edit = u64::MAX;
    for line in transcript.lines() {
        let Ok(e) = serde_json::from_str::<Event>(line) else {
            continue;
        };
        match e.kind.as_str() {
            "tool_call" => {
                f.tool_calls += 1;
                if recent.iter().any(|(n, a)| *n == e.name && *a == e.args) {
                    f.repeated_calls += 1;
                }
                recent.push_back((e.name.clone(), e.args.clone()));
                if recent.len() > 5 {
                    recent.pop_front();
                }
                if !seen_edit && (e.name == "write_file" || e.name == "edit_file") {
                    seen_edit = true;
                    f.calls_before_first_edit = f.tool_calls - 1;
                }
                f.last_tool = Some(e.name.clone());
                final_text.clear();
            }
            "tool_result" => {
                let entry = f.by_tool.entry(e.name.clone()).or_default();
                if e.ok {
                    entry.0 += 1
                } else {
                    entry.1 += 1
                }
                if e.output.contains("[killed after") {
                    f.timeouts += 1;
                }
                if e.output.contains("output truncated in the middle")
                    || e.output.contains(" bytes omitted ...]")
                {
                    f.truncations += 1;
                }
                let lower = e.output.to_lowercase();
                if lower.contains("cannot read")
                    || lower.contains("no such file")
                    || lower.contains("not found in")
                    || lower.contains("old_string not found")
                {
                    f.missing_paths += 1;
                }
            }
            "assistant" => final_text = e.text,
            "failed" => {
                f.failure = serde_json::from_value(e.reason).ok();
                f.ending = f
                    .failure
                    .as_ref()
                    .map_or_else(|| "unknown".into(), |failure| failure.kind.clone());
            }
            "settled" => f.ending = "settled".into(),
            "usage" => {
                f.input_tokens += e.input_tokens;
                f.output_tokens += e.output_tokens;
            }
            _ => {}
        }
        last_kind = e.kind;
    }
    f.no_settle = last_kind != "settled";
    if f.ending.is_empty() {
        f.ending = "unknown".into();
    }
    f.final_text_chars = final_text.chars().count() as u64;
    f.ended_deliberating = deliberating(&final_text);
    f
}

fn short(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

/// The witness's counts for the failed trials: what a transcript cannot
/// say about why a run ended.
pub fn render_observed(d: &Digest) -> String {
    let mut out = String::new();
    let failed: Vec<&TrialFacts> = d
        .trials
        .iter()
        .filter(|t| t.selection_reward() < 1.0)
        .collect();
    if failed.iter().all(|t| t.observed == Observed::default()) {
        return out;
    }
    out.push_str("\n### What the witness saw in failed trials\n\n| trial | trace | stream truncations | denials | provider retries | refusals | holds (denied) | cancelled in flight | endings | last failure |\n|---|---|---|---|---|---|---|---|---|---|\n");
    for t in failed {
        let o = &t.observed;
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} ({}) | {} | {} | {} |\n",
            t.trial,
            if o.complete { "complete" } else { "incomplete" },
            o.stream_truncations,
            o.denials,
            o.provider_retries,
            o.refusals,
            o.holds,
            o.held_denied,
            o.cancelled_in_flight,
            o.endings
                .iter()
                .map(|run| run.ending.as_str())
                .collect::<Vec<_>>()
                .join(" → "),
            o.last_failure
                .as_ref()
                .map(|failure| short(&failure.to_string(), 80))
                .unwrap_or_default()
        ));
    }
    out
}

fn mean(values: impl Iterator<Item = f64>, n: usize) -> f64 {
    if n == 0 {
        0.0
    } else {
        values.sum::<f64>() / n as f64
    }
}

pub fn aggregate(trials: &[&TrialFacts]) -> Aggregate {
    let n = trials.len();
    let mut by_tool: BTreeMap<String, f64> = BTreeMap::new();
    let mut last_tool: BTreeMap<String, u64> = BTreeMap::new();
    for t in trials {
        for (name, (ok, err)) in &t.by_tool {
            *by_tool.entry(name.clone()).or_default() += (ok + err) as f64 / n as f64;
        }
        if let Some(last) = &t.last_tool {
            *last_tool.entry(last.clone()).or_default() += 1;
        }
    }
    Aggregate {
        trials: n,
        tool_calls: mean(trials.iter().map(|t| t.tool_calls as f64), n),
        timeouts: mean(trials.iter().map(|t| t.timeouts as f64), n),
        truncations: mean(trials.iter().map(|t| t.truncations as f64), n),
        repeated_calls: mean(trials.iter().map(|t| t.repeated_calls as f64), n),
        missing_paths: mean(trials.iter().map(|t| t.missing_paths as f64), n),
        ended_deliberating: mean(
            trials
                .iter()
                .map(|t| if t.ended_deliberating { 1.0 } else { 0.0 }),
            n,
        ),
        no_settle: mean(
            trials.iter().map(|t| if t.no_settle { 1.0 } else { 0.0 }),
            n,
        ),
        calls_before_first_edit: mean(
            trials
                .iter()
                .filter(|t| t.calls_before_first_edit != u64::MAX)
                .map(|t| t.calls_before_first_edit as f64),
            trials
                .iter()
                .filter(|t| t.calls_before_first_edit != u64::MAX)
                .count(),
        ),
        input_tokens: mean(trials.iter().map(|t| t.input_tokens as f64), n),
        by_tool,
        last_tool,
    }
}

fn reward_of(trial_dir: &Path) -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string(trial_dir.join("verifier").join("reward.txt"))
        && let Ok(reward) = text.trim().parse::<f64>()
    {
        return Some(reward);
    }
    let Ok(text) = std::fs::read_to_string(trial_dir.join("result.json")) else {
        return None;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return None;
    };
    value["reward"]
        .as_f64()
        .or_else(|| value["verifier_result"]["rewards"]["reward"].as_f64())
}

/// Digest every trial under `job_dir`.
pub fn job(job_dir: &Path, harness_root: &Path) -> Result<Digest> {
    let mut trials = Vec::new();
    for entry in
        std::fs::read_dir(job_dir).with_context(|| format!("reading {}", job_dir.display()))?
    {
        let dir = entry?.path();
        let transcript = dir.join("agent").join("transcript.jsonl");
        if !transcript.is_file() {
            continue;
        }
        let mut f = facts(&std::fs::read_to_string(&transcript)?);
        if let Ok(trace) = std::fs::read_to_string(dir.join("agent").join("observations.json")) {
            f.observed = observed(&trace);
        }
        f.trial = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        (f.task, f.task_source, f.attempt) = metadata::trial(&dir, &f.trial);
        let reward = reward_of(&dir);
        f.unavailable_selection_reward = reward.unwrap_or(0.0);
        f.reward = reward.filter(|reward| reward.is_finite());
        trials.push(f);
    }
    trials.sort_by(|a, b| a.trial.cmp(&b.trial));
    let failed: Vec<&TrialFacts> = trials
        .iter()
        .filter(|t| t.selection_reward() < 1.0)
        .collect();
    let passed: Vec<&TrialFacts> = trials
        .iter()
        .filter(|t| t.selection_reward() >= 1.0)
        .collect();
    Ok(Digest {
        evaluation: metadata::evaluation(harness_root, job_dir),
        failed: aggregate(&failed),
        passed: aggregate(&passed),
        trials,
    })
}

/// The digest as the first section of a report.
pub fn render(d: &Digest) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "## Digest: {} failed trial(s) vs {} passed\n\n",
        d.failed.trials, d.passed.trials
    ));
    if d.trials.iter().any(|trial| trial.reward.is_none()) {
        out.push_str("The failed bucket includes unscored trials under the existing selection policy. A null reward means unavailable evidence, not a recorded task failure.\n\n");
    }
    out.push_str("### What failed trials did more of\n\n| fact (mean per trial) | failed | passed |\n|---|---|---|\n");
    let rows: [(&str, f64, f64); 9] = [
        ("tool calls", d.failed.tool_calls, d.passed.tool_calls),
        ("bash timeouts", d.failed.timeouts, d.passed.timeouts),
        (
            "truncated results",
            d.failed.truncations,
            d.passed.truncations,
        ),
        (
            "repeated identical calls",
            d.failed.repeated_calls,
            d.passed.repeated_calls,
        ),
        (
            "results naming a missing path",
            d.failed.missing_paths,
            d.passed.missing_paths,
        ),
        (
            "ended in deliberation, not a summary",
            d.failed.ended_deliberating,
            d.passed.ended_deliberating,
        ),
        (
            "ended without settling",
            d.failed.no_settle,
            d.passed.no_settle,
        ),
        (
            "calls before the first edit",
            d.failed.calls_before_first_edit,
            d.passed.calls_before_first_edit,
        ),
        ("input tokens", d.failed.input_tokens, d.passed.input_tokens),
    ];
    let mut sorted: Vec<&(&str, f64, f64)> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        let ra = if a.2 > 0.0 {
            a.1 / a.2
        } else if a.1 > 0.0 {
            f64::INFINITY
        } else {
            1.0
        };
        let rb = if b.2 > 0.0 {
            b.1 / b.2
        } else if b.1 > 0.0 {
            f64::INFINITY
        } else {
            1.0
        };
        rb.total_cmp(&ra)
    });
    for (name, failed, passed) in sorted {
        out.push_str(&format!("| {name} | {failed:.2} | {passed:.2} |\n"));
    }
    out.push_str(
        "\n### Calls per tool (mean per trial)\n\n| tool | failed | passed |\n|---|---|---|\n",
    );
    let mut tools: Vec<&String> = d
        .failed
        .by_tool
        .keys()
        .chain(d.passed.by_tool.keys())
        .collect();
    tools.sort();
    tools.dedup();
    for tool in tools {
        out.push_str(&format!(
            "| {tool} | {:.2} | {:.2} |\n",
            d.failed.by_tool.get(tool).copied().unwrap_or(0.0),
            d.passed.by_tool.get(tool).copied().unwrap_or(0.0)
        ));
    }
    out.push_str("\n### Failed trials\n\n| trial | calls | timeouts | truncated | repeats | missing paths | ending | last tool |\n|---|---|---|---|---|---|---|---|\n");
    for t in d.trials.iter().filter(|t| t.selection_reward() < 1.0) {
        let ending = if t.no_settle {
            format!("no settle: {}", short(&t.ending, 80))
        } else if t.ended_deliberating {
            "deliberating".to_owned()
        } else {
            "summary".to_owned()
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {ending} | {} |\n",
            t.trial,
            t.tool_calls,
            t.timeouts,
            t.truncations,
            t.repeated_calls,
            t.missing_paths,
            t.last_tool.as_deref().unwrap_or("-")
        ));
    }
    out.push('\n');
    out.push_str(&render_observed(d));
    out
}

pub fn write(job_dir: &Path, harness_root: &Path) -> Result<(Digest, PathBuf)> {
    let d = job(job_dir, harness_root)?;
    let path = job_dir.join("digest.json");
    std::fs::write(&path, serde_json::to_string_pretty(&d)?)?;
    Ok((d, path))
}

#[cfg(test)]
mod evidence_tests;

#[cfg(test)]
mod metadata_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, fields: &[(&str, serde_json::Value)]) -> String {
        let mut v = serde_json::json!({"kind": kind});
        for (k, val) in fields {
            v[k] = val.clone();
        }
        v.to_string()
    }

    fn call(name: &str, args: &str) -> String {
        line("tool_call", &[("name", name.into()), ("args", args.into())])
    }
    fn result(name: &str, output: &str, ok: bool) -> String {
        line(
            "tool_result",
            &[
                ("name", name.into()),
                ("output", output.into()),
                ("ok", ok.into()),
            ],
        )
    }

    #[test]
    fn counts_every_fact() {
        let t = [
            line("user", &[("text", "do it".into())]),
            call("bash", "{\"command\":\"find /\"}"),
            result(
                "bash",
                "...\n[killed after 120s timeout]\n[exit code: signal]",
                true,
            ),
            call("read_file", "{\"path\":\"x\"}"),
            result("read_file", "error: cannot read /app/x: No such file", true),
            call("read_file", "{\"path\":\"x\"}"),
            result("read_file", "error: cannot read /app/x: No such file", true),
            call("bash", "{\"command\":\"cat big\"}"),
            result(
                "bash",
                "head\n\n[... output truncated in the middle ...]\n\ntail",
                true,
            ),
            call("edit_file", "{\"path\":\"a\"}"),
            result("edit_file", "1 replacement(s)", true),
            line(
                "assistant",
                &[(
                    "text",
                    "Should I use cwe-93 or CWE-93? Let me think.".into(),
                )],
            ),
            line(
                "usage",
                &[("input_tokens", 100.into()), ("output_tokens", 7.into())],
            ),
            line("settled", &[("answer", "".into())]),
        ]
        .join("\n");
        let f = facts(&t);
        assert_eq!(f.tool_calls, 5);
        assert_eq!(f.timeouts, 1);
        assert_eq!(f.truncations, 1);
        assert_eq!(f.repeated_calls, 1);
        assert_eq!(f.missing_paths, 2);
        assert_eq!(f.calls_before_first_edit, 4);
        assert!(f.ended_deliberating);
        assert!(!f.no_settle);
        assert_eq!(f.by_tool["bash"], (2, 0));
        assert_eq!(f.input_tokens, 100);
        assert_eq!(f.last_tool.as_deref(), Some("edit_file"));
    }

    #[test]
    fn a_summary_ending_is_not_deliberation_and_a_failed_run_did_not_settle() {
        let ok = [
            call("bash", "{}"),
            result("bash", "ok", true),
            line(
                "assistant",
                &[("text", "Fixed add() and ran the test; it passes.".into())],
            ),
            line("settled", &[]),
        ]
        .join("\n");
        let f = facts(&ok);
        assert!(!f.ended_deliberating);
        assert!(!f.no_settle);
        let failed = [
            call("bash", "{}"),
            result("bash", "ok", true),
            line("failed", &[]),
        ]
        .join("\n");
        let f = facts(&failed);
        assert!(f.no_settle);
        assert_eq!(f.calls_before_first_edit, u64::MAX);
    }

    #[test]
    fn aggregate_means_and_render_order() {
        let mut a = facts(
            &[
                call("bash", "{}"),
                result("bash", "[killed after 1s timeout]", true),
                line("settled", &[]),
            ]
            .join("\n"),
        );
        a.reward = Some(0.0);
        let mut b = facts(
            &[
                call("bash", "{}"),
                result("bash", "fine", true),
                line("settled", &[]),
            ]
            .join("\n"),
        );
        b.reward = Some(1.0);
        let d = Digest {
            evaluation: None,
            failed: aggregate(&[&a]),
            passed: aggregate(&[&b]),
            trials: vec![a, b],
        };
        assert_eq!(d.failed.timeouts, 1.0);
        assert_eq!(d.passed.timeouts, 0.0);
        let text = render(&d);
        let first_row = text
            .lines()
            .find(|l| l.starts_with("| ") && !l.starts_with("| fact"))
            .unwrap();
        assert!(first_row.starts_with("| bash timeouts"), "{first_row}");
    }
}

/// Does the final text read as thinking rather than a summary? Two or more
/// questions, a deliberation phrase in its last 1500 characters, or a
/// cut-off ending (no terminal punctuation on a text of any length).
fn deliberating(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    let questions = trimmed.matches('?').count();
    let lower = trimmed.to_lowercase();
    let tail: String = lower
        .chars()
        .rev()
        .take(1500)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let cut_off = trimmed.chars().count() > 200
        && !trimmed.ends_with(['.', '!', '`', '"', '*', ')', ']', '>']);
    questions >= 2 || DELIBERATION.iter().any(|p| tail.contains(p)) || cut_off
}

#[cfg(test)]
mod deliberation_tests {
    use super::{deliberating, facts};

    #[test]
    fn the_real_gen1_ending_is_deliberation() {
        // The tail of the fix-code-vulnerability final text from the first
        // evolve run's generation 1: the model weighed cwe-93 against
        // CWE-93 and stopped mid-sentence without writing the report.
        let text = "_cwe) == 2 != 1`, which FAILS!\nSo having 2 elements is definitely risky if strict equality is used.\nWe should have exactly 1 element in `cwe_id`.\n\nNow, which 1 element: `\"cwe-93\"` or `\"CWE-93\"`?\nLet's compare:\nArgument for `\"cwe-93\"`:\n- The instruction says:\n  `- demonstration of format of vulnerable item: {\"file_path\": \"/app/example.cpp\", \"cwe_id\": [\"cwe-123\"]}`\n  It literally gives you the format of the vulnerable item!\n  Format: `\"cwe-123\"` -> lowercase!\n  If you give an instruction with an example of format, and an agent follows the demonstration and writes `{\"file_path\": \"/app/bottle.py\", \"cwe_id\": [\"cwe-93\"]}`,\n  that is 100";
        assert!(deliberating(text));
    }

    #[test]
    fn a_summary_is_not() {
        assert!(!deliberating(
            "Fixed the header validation in bottle.py and wrote /app/report.jsonl; all 367 tests pass."
        ));
        assert!(!deliberating(
            "Done. The test suite passes (`pytest -q`: 12 passed)."
        ));
        assert!(!deliberating(""));
    }

    #[test]
    fn bounded_shell_capture_is_counted_as_truncated() {
        let transcript = serde_json::json!({"kind":"tool_result", "name":"bash", "ok":true, "output":"head\n[... 19976000 bytes omitted ...]\ntail"}).to_string();
        assert_eq!(facts(&transcript).truncations, 1);
    }
}
