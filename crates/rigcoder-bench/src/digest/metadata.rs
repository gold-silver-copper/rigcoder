//! Existing harness identity, without inferring evaluated code from Git HEAD.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSource {
    ResultFile,
    DirectoryName,
}

/// The available subset of a uniquely matched evaluation ledger row.
/// Model/slice/attempts are partial settings, not a complete configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationMetadata {
    pub job_dir: String,
    pub generation: Option<usize>,
    pub slice: String,
    /// The ledger's commit can precede uncommitted, rejected candidate edits.
    pub recorded_commit: String,
    /// Revision of the editor, not the evaluated binary.
    pub meta_commit: Option<String>,
    pub model: String,
    pub attempts: usize,
    pub lane: Option<crate::evolve::Lane>,
    /// The existing ledger does not establish an evaluated candidate revision.
    pub candidate_revision: Option<String>,
    /// No complete configuration identifier is recorded by the existing harness.
    pub configuration_id: Option<String>,
}

pub(super) fn evaluation(root: &Path, job_dir: &Path) -> Option<EvaluationMetadata> {
    let target = job_dir.canonicalize().ok()?;
    let text = std::fs::read_to_string(root.join("harness/ledger.jsonl")).ok()?;
    let mut matched = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        // A malformed row may hide a second match. Do not silently skip it and
        // claim unique attribution from the remaining rows.
        let entry: crate::ledger::Entry = serde_json::from_str(line).ok()?;
        let path = root.join(&entry.job_dir);
        if path.canonicalize().ok().as_ref() != Some(&target) {
            continue;
        }
        if matched.is_some() {
            return None;
        }
        matched = Some(EvaluationMetadata {
            job_dir: entry.job_dir,
            generation: entry.generation,
            slice: entry.slice,
            recorded_commit: entry.commit,
            meta_commit: entry.meta_commit,
            model: entry.model,
            attempts: entry.attempts,
            lane: entry.lane,
            candidate_revision: None,
            configuration_id: None,
        });
    }
    matched
}

pub(super) fn trial(
    dir: &Path,
    directory_name: &str,
) -> (Option<String>, Option<TaskSource>, Option<usize>) {
    #[derive(Deserialize)]
    struct Identity {
        task: Option<String>,
        attempt: Option<usize>,
    }
    let identity = std::fs::read_to_string(dir.join("result.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Identity>(&text).ok());
    let attempt = identity.as_ref().and_then(|identity| identity.attempt);
    if let Some(task) = identity
        .and_then(|identity| identity.task)
        .filter(|task| !task.is_empty())
    {
        return (Some(task), Some(TaskSource::ResultFile), attempt);
    }
    match directory_name.rsplit_once("__") {
        Some((task, _)) if !task.is_empty() => (
            Some(task.to_owned()),
            Some(TaskSource::DirectoryName),
            attempt,
        ),
        _ => (None, None, attempt),
    }
}
