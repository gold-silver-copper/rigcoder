//! `harness/ledger.jsonl`: one entry per evaluation, appended.

use std::{io::Write, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::stats::{Decision, Summary};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// `None` for a holdout evaluation.
    pub generation: Option<usize>,
    /// `dev`, `holdout`, or `custom` for explicit task lists.
    pub slice: String,
    pub commit: String,
    pub decision: Option<Decision>,
    pub best_score: f64,
    pub best_ci_low: f64,
    pub model: String,
    pub attempts: usize,
    /// The lane whose self-edit this generation measured (None for a baseline or holdout).
    #[serde(default)]
    pub lane: Option<crate::evolve::Lane>,
    /// The commit the meta agent ran as when it produced this generation's edit.
    #[serde(default)]
    pub meta_commit: Option<String>,
    pub job_dir: String,
    pub time: u64,
    #[serde(flatten)]
    pub summary: Summary,
}

fn path(root: &Path) -> std::path::PathBuf {
    root.join("harness").join("ledger.jsonl")
}

pub fn append(root: &Path, entry: &Entry) -> Result<()> {
    let line = serde_json::to_string(entry)?;
    println!("{line}");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path(root))
        .context("opening the ledger")?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn read(root: &Path) -> Result<Vec<Entry>> {
    let path = path(root);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for (index, line) in std::fs::read_to_string(&path)?.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // Entries from before the score had an interval are skipped, not
        // fatal: the ledger is append-only history.
        match serde_json::from_str::<Entry>(line) {
            Ok(entry) => entries.push(entry),
            Err(error) => eprintln!("ledger line {}: skipped ({error})", index + 1),
        }
    }
    Ok(entries)
}

/// The best kept dev-slice evaluation so far: the highest lower bound.
pub fn best_kept(root: &Path) -> Result<Option<Entry>> {
    Ok(read(root)?
        .into_iter()
        .filter(|e| e.slice == "dev" && matches!(e.decision, Some(Decision::Kept | Decision::Tie)))
        .max_by(|a, b| a.summary.ci_low.total_cmp(&b.summary.ci_low)))
}

pub fn print(root: &Path) -> Result<()> {
    println!("{:<4} {:<8} {:<9} {:<9} {:<8} {:>6} {:>6} {:>6} {:>13} {:>7}", "gen", "slice", "commit", "decision", "lane", "score", "pass1", "passk", "95% CI", "trials");
    for e in read(root)? {
        println!(
            "{:<4} {:<8} {:<9} {:<9} {:<8} {:>6.3} {:>6.3} {:>6.3} {:>6.3}–{:<6.3} {:>7}",
            e.generation.map_or("-".to_owned(), |g| g.to_string()),
            e.slice,
            e.commit,
            e.decision.map_or("-".to_owned(), |d| format!("{d:?}").to_lowercase()),
            e.lane.map_or("-", |l| l.name()),
            e.summary.score,
            e.summary.pass1,
            e.summary.passk,
            e.summary.ci_low,
            e.summary.ci_high,
            e.summary.trials
        );
    }
    Ok(())
}
