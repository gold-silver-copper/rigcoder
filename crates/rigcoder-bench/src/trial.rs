//! One trial: a task, an attempt number, a container, a reward.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{docker, task::Task};

/// What a trial leaves behind, in `<job>/<task>__<attempt>/result.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialRecord {
    pub task: String,
    pub attempt: usize,
    pub reward: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub tool_calls: u64,
    pub wall_seconds: f64,
    pub settled: bool,
    /// A harness-side failure (build, container, verifier), not the agent's.
    pub error: Option<String>,
}

pub struct TrialSpec<'a> {
    pub task: &'a Task,
    pub attempt: usize,
    pub job_dir: &'a Path,
    pub binary: &'a Path,
    pub provider: &'a str,
    pub model: &'a str,
    pub api_key: Option<(&'a str, &'a str)>,
    pub max_turns: usize,
}

const REMOTE_BIN: &str = "/usr/local/bin/rigcoder";

pub fn trial_dir(job_dir: &Path, task: &str, attempt: usize) -> PathBuf {
    job_dir.join(format!("{task}__{attempt}"))
}

/// Build the task image once per job (docker's own cache makes repeats cheap).
pub fn build_image(task: &Task) -> Result<()> {
    let out = docker::build(&task.dir.join("environment"), &task.image_tag(), Duration::from_secs(task.build_timeout_secs))?;
    if out.code != 0 {
        bail!("image build for {} failed (exit {}): {}", task.name, out.code, last_lines(&out.stderr, 20));
    }
    Ok(())
}

pub fn run(spec: &TrialSpec) -> TrialRecord {
    let dir = trial_dir(spec.job_dir, &spec.task.name, spec.attempt);
    let _ = std::fs::create_dir_all(dir.join("agent"));
    let _ = std::fs::create_dir_all(dir.join("verifier"));
    let started = Instant::now();
    let container = format!("rigcoder-{}-{}-{}", spec.task.name, spec.attempt, std::process::id());
    let outcome = execute(spec, &container, &dir);
    docker::remove(&container);
    let mut record = read_agent_output(&dir, &spec.task.name, spec.attempt);
    record.wall_seconds = started.elapsed().as_secs_f64();
    match outcome {
        Ok(reward) => record.reward = reward,
        Err(error) => {
            record.error = Some(format!("{error:#}"));
            record.reward = 0.0;
        }
    }
    if let Ok(json) = serde_json::to_string_pretty(&record) {
        let _ = std::fs::write(dir.join("result.json"), json);
    }
    record
}

fn execute(spec: &TrialSpec, container: &str, dir: &Path) -> Result<f64> {
    let task = spec.task;
    docker::start(container, &task.image_tag(), &task.workdir, task.cpus, &task.memory)?;
    let setup = docker::exec(
        container,
        "/",
        &[],
        "mkdir -p /logs/agent /logs/verifier /tests && \
         ((command -v bash >/dev/null && [ -s /etc/ssl/certs/ca-certificates.crt ]) \
          || (apt-get update -qq && apt-get install -y -qq bash ca-certificates) || true)",
        Duration::from_secs(300),
    )?;
    if setup.code != 0 {
        bail!("container setup failed: {}", last_lines(&setup.stderr, 10));
    }
    docker::copy_in(container, spec.binary, REMOTE_BIN)?;
    let _ = docker::exec(container, "/", &[], &format!("chmod 755 {REMOTE_BIN}"), Duration::from_secs(30))?;
    // The task's tests, and the instruction as a file so no quoting is needed.
    docker::copy_in(container, &task.dir.join("tests"), "/tests")?;
    // `docker cp dir /tests` nests when /tests exists; flatten.
    let _ = docker::exec(container, "/", &[], "if [ -d /tests/tests ]; then cp -r /tests/tests/. /tests/ && rm -rf /tests/tests; fi", Duration::from_secs(30))?;
    let instruction_file = dir.join("instruction.md");
    std::fs::write(&instruction_file, &task.instruction)?;
    docker::copy_in(container, &instruction_file, "/logs/agent/instruction.md")?;

    let timeout_secs = task.agent_timeout_secs.saturating_sub(60).max(60).to_string();
    let max_turns = spec.max_turns.to_string();
    let mut env: Vec<(&str, &str)> = vec![
        ("RIGCODER_PROVIDER", spec.provider),
        ("RIGCODER_MODEL", spec.model),
        ("RUST_LOG", "warn"),
    ];
    if let Some((key, value)) = spec.api_key {
        env.push((key, value));
    }
    let agent = docker::exec(
        container,
        &task.workdir,
        &env,
        &format!(
            "{REMOTE_BIN} --cwd {} --max-turns {max_turns} --timeout-secs {timeout_secs} \
             --transcript /logs/agent/transcript.jsonl --effect-log /logs/agent/effects.json --task-file /logs/agent/instruction.md \
             > /logs/agent/rigcoder.txt 2>&1; echo $? > /logs/agent/exit_code.txt",
            task.workdir
        ),
        Duration::from_secs(task.agent_timeout_secs),
    )?;
    let _ = docker::copy_out(container, "/logs/agent/.", &dir.join("agent"));
    if agent.code == 137 {
        eprintln!("[{}#{}] agent hit the hard timeout", task.name, spec.attempt);
    }

    let verifier = docker::exec(
        container,
        &task.workdir,
        &[],
        "bash /tests/test.sh > /logs/verifier/output.txt 2>&1; echo $? > /logs/verifier/exit_code.txt",
        Duration::from_secs(task.verifier_timeout_secs),
    )?;
    let _ = docker::copy_out(container, "/logs/verifier/.", &dir.join("verifier"));
    if verifier.code == 137 {
        bail!("verifier hit its timeout");
    }
    let reward = docker::read_file(container, "/logs/verifier/reward.txt")?
        .with_context(|| "verifier wrote no reward.txt")?;
    reward
        .trim()
        .parse::<f64>()
        .with_context(|| format!("reward.txt is not a number: {reward:?}"))
}

/// Tokens, tool calls and the ending, from the agent's transcript.
fn read_agent_output(dir: &Path, task: &str, attempt: usize) -> TrialRecord {
    let mut record = TrialRecord {
        task: task.to_owned(),
        attempt,
        reward: 0.0,
        input_tokens: 0,
        output_tokens: 0,
        cache_tokens: 0,
        tool_calls: 0,
        wall_seconds: 0.0,
        settled: false,
        error: None,
    };
    let Ok(text) = std::fs::read_to_string(dir.join("agent").join("transcript.jsonl")) else {
        return record;
    };
    for line in text.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match event.get("kind").and_then(|k| k.as_str()) {
            Some("tool_call") => record.tool_calls += 1,
            Some("settled") => record.settled = true,
            Some("usage") => {
                record.input_tokens += event["input_tokens"].as_u64().unwrap_or(0);
                record.output_tokens += event["output_tokens"].as_u64().unwrap_or(0);
                record.cache_tokens += event["cached_input_tokens"].as_u64().unwrap_or(0);
            }
            _ => {}
        }
    }
    record
}

/// Every trial record under a job directory.
pub fn read_job(job_dir: &Path) -> Result<Vec<TrialRecord>> {
    let mut records = Vec::new();
    for entry in std::fs::read_dir(job_dir).with_context(|| format!("reading {}", job_dir.display()))? {
        let path = entry?.path().join("result.json");
        if path.is_file() {
            let record: TrialRecord = serde_json::from_str(&std::fs::read_to_string(&path)?)
                .with_context(|| path.display().to_string())?;
            records.push(record);
        }
    }
    records.sort_by(|a, b| (&a.task, a.attempt).cmp(&(&b.task, b.attempt)));
    Ok(records)
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
