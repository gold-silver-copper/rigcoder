//! The failure report the meta agent reads.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{stats::Summary, trial::{TrialRecord, trial_dir}};

fn tail(path: &Path, chars: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) if text.len() <= chars => text,
        Ok(text) => {
            let start = text.len() - chars;
            let start = (start..text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(text.len());
            format!("…{}", &text[start..])
        }
        Err(_) => "(missing)".to_owned(),
    }
}

pub fn write(job_dir: &Path, generation: usize, summary: &Summary, trials: &[TrialRecord], best_score: f64, best_low: f64) -> Result<PathBuf> {
    let mut out = String::new();
    out.push_str(&format!("# Generation {generation}\n\n"));
    out.push_str(&format!(
        "Score {:.3} (95% CI {:.3}–{:.3}), pass@1 {:.3}, pass@k {:.3} over {} trial(s) of {} task(s). Best kept so far: {:.3} (lower bound {:.3}).\n\n",
        summary.score, summary.ci_low, summary.ci_high, summary.pass1, summary.passk, summary.trials, summary.tasks, best_score, best_low
    ));
    out.push_str("| task | attempts |\n|---|---|\n");
    for (task, rewards) in &summary.rewards {
        let attempts: Vec<String> = rewards.iter().map(|r| format!("{r:.1}")).collect();
        out.push_str(&format!("| {task} | {} |\n", attempts.join(" ")));
    }
    out.push('\n');
    for t in trials.iter().filter(|t| t.reward < 1.0) {
        let dir = trial_dir(job_dir, &t.task, t.attempt);
        out.push_str(&format!("## Failed: {} (attempt {})\n\n", t.task, t.attempt));
        if let Some(error) = &t.error {
            out.push_str(&format!("Harness error: {error}\n\n"));
        }
        out.push_str(&format!("### Instruction\n\n{}\n\n", tail(&dir.join("instruction.md"), 3000)));
        out.push_str(&format!("### Agent transcript (tail)\n\n```\n{}\n```\n\n", tail(&dir.join("agent").join("rigcoder.txt"), 6000)));
        out.push_str(&format!("### Verifier output\n\n```\n{}\n```\n\n", tail(&dir.join("verifier").join("output.txt"), 3000)));
    }
    let path = job_dir.join("report.md");
    std::fs::write(&path, out)?;
    Ok(path)
}
