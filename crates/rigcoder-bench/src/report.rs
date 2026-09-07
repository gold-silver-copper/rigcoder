//! The failure report the meta agent reads: the digest first, then every
//! failed trial in full (instruction, transcript with long results
//! collapsed, verifier output), capped at 400 KB.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use crate::{
    digest,
    stats::Summary,
    trial::{TrialRecord, trial_dir},
};

const REPORT_CAP: usize = 400 * 1024;
const COLLAPSE_LINES: usize = 10;

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| "(missing)".to_owned())
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
    answer: String,
}

/// Long tool results keep their first and last lines.
fn collapse(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 2 * COLLAPSE_LINES {
        return text.to_owned();
    }
    let mut out: Vec<&str> = lines[..COLLAPSE_LINES].to_vec();
    let hidden = lines.len() - 2 * COLLAPSE_LINES;
    let marker = format!("[... {hidden} lines ...]");
    out.push(&marker);
    out.extend(&lines[lines.len() - COLLAPSE_LINES..]);
    out.join("\n")
}

/// The whole transcript as text, every event in order.
pub fn render_transcript(jsonl: &str) -> String {
    let mut out = String::new();
    for line in jsonl.lines() {
        let Ok(e) = serde_json::from_str::<Event>(line) else { continue };
        match e.kind.as_str() {
            "user" => out.push_str(&format!("> {}\n\n", e.text)),
            "assistant" => out.push_str(&format!("{}\n\n", e.text)),
            "tool_call" => out.push_str(&format!("[tool] {} {}\n", e.name, e.args)),
            "tool_result" => out.push_str(&format!("[{}] {}\n{}\n\n", if e.ok { "result" } else { "error" }, e.name, collapse(&e.output))),
            "settled" => out.push_str(&format!("[settled] {}\n", collapse(&e.answer))),
            "failed" => out.push_str("[failed]\n"),
            other => out.push_str(&format!("[{other}]\n")),
        }
    }
    out
}

pub fn write(job_dir: &Path, generation: usize, summary: &Summary, trials: &[TrialRecord], best_score: f64, best_low: f64) -> Result<PathBuf> {
    let (d, _) = digest::write(job_dir)?;
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
    out.push_str(&digest::render(&d));

    // Failed trials in full. Verifier output is never dropped; if the cap
    // is hit, transcripts are shortened from the longest down.
    let failed: Vec<&TrialRecord> = trials.iter().filter(|t| t.reward < 1.0).collect();
    let mut sections: Vec<(String, String, String)> = failed
        .iter()
        .map(|t| {
            let dir = trial_dir(job_dir, &t.task, t.attempt);
            let mut head = format!("## Failed: {} (attempt {})\n\n", t.task, t.attempt);
            if let Some(error) = &t.error {
                head.push_str(&format!("Harness error: {error}\n\n"));
            }
            head.push_str(&format!("### Instruction\n\n{}\n\n", read(&dir.join("instruction.md"))));
            let transcript = format!(
                "### Agent transcript (full; long results collapsed; JSON at {})\n\n```\n{}\n```\n\n",
                dir.join("agent").join("transcript.jsonl").display(),
                render_transcript(&read(&dir.join("agent").join("transcript.jsonl")))
            );
            let verifier = format!("### Verifier output\n\n```\n{}\n```\n\n", read(&dir.join("verifier").join("output.txt")));
            (head, transcript, verifier)
        })
        .collect();
    let fixed: usize = out.len() + sections.iter().map(|(h, _, v)| h.len() + v.len()).sum::<usize>();
    let mut budget = REPORT_CAP.saturating_sub(fixed);
    let total_transcripts: usize = sections.iter().map(|(_, t, _)| t.len()).sum();
    if total_transcripts > budget && !sections.is_empty() {
        let each = budget / sections.len();
        for (_, transcript, _) in &mut sections {
            if transcript.len() > each {
                let keep = transcript.len().saturating_sub(each).max(0);
                let start = (keep..transcript.len()).find(|i| transcript.is_char_boundary(*i)).unwrap_or(transcript.len());
                *transcript = format!("### Agent transcript (tail, cut for the report cap)\n\n```\n…{}", &transcript[start..]);
            }
        }
        budget = 0;
    }
    let _ = budget;
    for (head, transcript, verifier) in sections {
        out.push_str(&head);
        out.push_str(&transcript);
        out.push_str(&verifier);
    }
    let path = job_dir.join("report.md");
    std::fs::write(&path, out)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_results_collapse_to_head_and_tail() {
        let text: String = (1..=40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let c = collapse(&text);
        assert!(c.starts_with("line 1\n"));
        assert!(c.ends_with("line 40"));
        assert!(c.contains("[... 20 lines ...]"));
        assert_eq!(collapse("short"), "short");
    }
}
