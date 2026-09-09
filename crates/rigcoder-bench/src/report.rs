//! The failure report the meta agent reads: the digest first, then every
//! failed trial in full (instruction, transcript with long results
//! collapsed, verifier output), capped at 400 KiB with complete evidence on disk.

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
        let Ok(e) = serde_json::from_str::<Event>(line) else {
            continue;
        };
        match e.kind.as_str() {
            "user" => out.push_str(&format!("> {}\n\n", e.text)),
            "assistant" => out.push_str(&format!("{}\n\n", e.text)),
            "tool_call" => out.push_str(&format!("[tool] {} {}\n", e.name, e.args)),
            "tool_result" => out.push_str(&format!(
                "[{}] {}\n{}\n\n",
                if e.ok { "result" } else { "error" },
                e.name,
                collapse(&e.output)
            )),
            "settled" => out.push_str(&format!("[settled] {}\n", collapse(&e.answer))),
            "failed" => out.push_str("[failed]\n"),
            other => out.push_str(&format!("[{other}]\n")),
        }
    }
    out
}

pub fn write(
    job_dir: &Path,
    harness_root: &Path,
    generation: usize,
    summary: &Summary,
    trials: &[TrialRecord],
    best_score: f64,
    best_low: f64,
) -> Result<PathBuf> {
    let (d, _) = digest::write(job_dir, harness_root)?;
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

    // Keep complete evidence on disk. Large reports expose bounded excerpts
    // with a link to the full report instead of silently dropping verifier data.
    for t in trials.iter().filter(|t| t.reward < 1.0) {
        let dir = trial_dir(job_dir, &t.task, t.attempt);
        out.push_str(&format!(
            "## Failed: {} (attempt {})\n\n",
            t.task, t.attempt
        ));
        if let Some(error) = &t.error {
            out.push_str(&format!("Harness error: {error}\n\n"));
        }
        out.push_str("### Instruction\n\n");
        out.push_str(&fenced(&read(&dir.join("instruction.md"))));
        out.push_str(&format!(
            "### Agent transcript (long results collapsed; JSON at {})\n\n",
            dir.join("agent/transcript.jsonl").display()
        ));
        out.push_str(&fenced(&render_transcript(&read(
            &dir.join("agent/transcript.jsonl"),
        ))));
        out.push_str("### Verifier output\n\n");
        out.push_str(&fenced(&read(&dir.join("verifier/output.txt"))));
    }
    if out.len() > REPORT_CAP {
        std::fs::write(job_dir.join("report.full.md"), &out)?;
        out = bounded_excerpt(&out, REPORT_CAP);
    }
    let path = job_dir.join("report.md");
    std::fs::write(&path, out)?;
    Ok(path)
}

fn fenced(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.saturating_add(1).max(3));
    format!("{fence}\n{text}\n{fence}\n\n")
}

fn indented_prefix(text: &str, budget: usize) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if budget.saturating_sub(out.len()) <= 5 {
            break;
        }
        out.push_str("    ");
        let mut end = line.len().min(budget - out.len() - 1);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        out.push_str(&line[..end]);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if end < line.len() {
            break;
        }
    }
    out
}

fn indented_suffix(text: &str, budget: usize) -> String {
    let mut parts = Vec::new();
    let mut remaining = budget;
    for line in text.lines().rev() {
        if remaining <= 5 {
            break;
        }
        let mut start = line.len().saturating_sub(remaining - 5);
        while !line.is_char_boundary(start) {
            start += 1;
        }
        let part = format!("    {}\n", &line[start..]);
        remaining -= part.len();
        parts.push(part);
        if start > 0 {
            break;
        }
    }
    parts.reverse();
    parts.concat()
}

fn bounded_excerpt(text: &str, cap: usize) -> String {
    let header = "# Report excerpt\n\nThe report exceeded its size limit. [Read the complete report](report.full.md) for all instructions, transcripts and verifier output.\n\n";
    let marker = "\nContent omitted; complete evidence is in report.full.md.\n\n";
    let budget = cap.saturating_sub(header.len() + marker.len());
    let mut out = header.to_owned();
    out.push_str(&indented_prefix(text, budget / 2));
    out.push_str(marker);
    out.push_str(&indented_suffix(text, cap.saturating_sub(out.len())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_results_collapse_to_head_and_tail() {
        let text: String = (1..=40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let c = collapse(&text);
        assert!(c.starts_with("line 1\n"));
        assert!(c.ends_with("line 40"));
        assert!(c.contains("[... 20 lines ...]"));
        assert_eq!(collapse("short"), "short");
    }

    #[test]
    fn oversized_unicode_evidence_is_bounded_and_has_no_open_fence() {
        let text = format!(
            "```\n{}\n```\n{}",
            "界\n".repeat(REPORT_CAP),
            "verifier".repeat(REPORT_CAP)
        );
        let report = bounded_excerpt(&text, REPORT_CAP);
        assert!(report.len() <= REPORT_CAP);
        assert!(report.contains("report.full.md"));
        assert!(!report.lines().any(|line| line.starts_with("```")));
        assert!(report.contains("verifier"));
    }

    #[test]
    fn embedded_backticks_cannot_close_evidence_fence() {
        let rendered = fenced("```\nmodel output\n```");
        assert!(rendered.starts_with("````\n"));
        assert!(rendered.ends_with("\n````\n\n"));
    }

    #[test]
    fn large_verifier_keeps_complete_evidence_beside_bounded_report() {
        let root = std::env::temp_dir().join(format!(
            "rigcoder-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let trial = root.join("case__1");
        std::fs::create_dir_all(trial.join("agent")).unwrap();
        std::fs::create_dir_all(trial.join("verifier")).unwrap();
        let record = TrialRecord {
            task: "case".to_owned(),
            attempt: 1,
            reward: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cache_tokens: 0,
            tool_calls: 0,
            wall_seconds: 0.0,
            settled: false,
            error: None,
        };
        std::fs::write(
            trial.join("result.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        std::fs::write(trial.join("instruction.md"), "repair the task").unwrap();
        std::fs::write(trial.join("agent/transcript.jsonl"), "").unwrap();
        let verifier = format!("{}END-OF-VERIFIER", "界\nx\n".repeat(REPORT_CAP));
        std::fs::write(trial.join("verifier/output.txt"), &verifier).unwrap();
        let report = write(&root, &root, 0, &Summary::default(), &[record], 0.0, 0.0).unwrap();
        let text = std::fs::read_to_string(report).unwrap();
        assert!(text.len() <= REPORT_CAP);
        assert!(text.contains("report.full.md"));
        assert!(text.contains("END-OF-VERIFIER"));
        assert!(
            std::fs::read_to_string(root.join("report.full.md"))
                .unwrap()
                .contains(&verifier)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
