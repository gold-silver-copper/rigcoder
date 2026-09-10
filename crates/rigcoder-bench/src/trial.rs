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
    /// Sum of reported usage; null when absent, malformed or overflowing.
    /// Even a known sum does not prove every provider attempt reported usage.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_tokens: Option<u64>,
    pub tool_calls: u64,
    pub wall_seconds: f64,
    pub settled: bool,
    /// A harness-side failure (build, container, verifier), not the agent's.
    pub error: Option<String>,
}

pub struct TrialSpec<'a> {
    pub task: &'a Task,
    pub image: &'a str,
    pub attempt: usize,
    pub job_dir: &'a Path,
    pub binary: &'a Path,
    pub provider: &'a str,
    pub model: &'a str,
    pub api_key: Option<(&'a str, &'a str)>,
    pub budget: Option<&'a Path>,
    pub budget_phase: &'a str,
    pub max_turns: usize,
    pub checkpoint: bool,
}

const REMOTE_BIN: &str = "/usr/local/bin/rigcoder";

pub fn trial_dir(job_dir: &Path, task: &str, attempt: usize) -> PathBuf {
    job_dir.join(format!("{task}__{attempt}"))
}

/// Build the task image once per job (docker's own cache makes repeats cheap).
pub fn build_image(task: &Task, receipt: &Path) -> Result<String> {
    if let Some(parent) = receipt.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(receipt) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let out = docker::build(
        &task.dir.join("environment"),
        &task.image_tag(),
        receipt,
        Duration::from_secs(task.build_timeout_secs),
    )?;
    if out.code != 0 {
        bail!(
            "image build for {} failed (exit {}): {}",
            task.name,
            out.code,
            last_lines(&out.stderr, 20)
        );
    }
    let image = std::fs::read_to_string(receipt).context("build produced no image ID")?;
    let image = image.trim();
    anyhow::ensure!(
        image
            .strip_prefix("sha256:")
            .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())),
        "build produced invalid image ID"
    );
    Ok(image.to_owned())
}

pub fn run(spec: &TrialSpec) -> TrialRecord {
    let dir = trial_dir(spec.job_dir, &spec.task.name, spec.attempt);
    let started = Instant::now();
    let container = format!(
        "rigcoder-{}-{}-{}",
        spec.task.name,
        spec.attempt,
        std::process::id()
    );
    let outcome = (|| {
        std::fs::create_dir_all(dir.join("agent"))?;
        std::fs::create_dir_all(dir.join("verifier"))?;
        if let Some(budget) = spec.budget {
            std::fs::write(
                dir.join("budget-link.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "ledger": budget, "context": dir, "phase": spec.budget_phase,
                }))?,
            )?;
        }
        execute(spec, &container, &dir)
    })();
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
    if let Err(error) = serde_json::to_string_pretty(&record)
        .map_err(anyhow::Error::from)
        .and_then(|json| std::fs::write(dir.join("result.json"), json).map_err(anyhow::Error::from))
    {
        record.error = Some(format!("saving trial record: {error:#}"));
    }
    record
}

fn execute(spec: &TrialSpec, container: &str, dir: &Path) -> Result<f64> {
    let task = spec.task;
    let start = if spec.budget.is_some() {
        docker::start_isolated
    } else {
        docker::start
    };
    start(
        container,
        spec.image,
        &task.workdir,
        task.cpus,
        &task.memory,
    )?;
    let setup = docker::exec(
        container,
        "/",
        &[],
        if spec.budget.is_some() {
            "mkdir -p /logs/agent /logs/verifier /tests && command -v bash && command -v python3"
        } else {
            "mkdir -p /logs/agent /logs/verifier /tests && \
         ((command -v bash >/dev/null && [ -s /etc/ssl/certs/ca-certificates.crt ]) \
          || (apt-get update -qq && apt-get install -y -qq bash ca-certificates) || true)"
        },
        Duration::from_secs(300),
    )?;
    if setup.code != 0 {
        bail!("container setup failed: {}", last_lines(&setup.stderr, 10));
    }
    docker::copy_in(container, spec.binary, REMOTE_BIN)?;
    let chmod = docker::exec(
        container,
        "/",
        &[],
        &format!("chmod 755 {REMOTE_BIN}"),
        Duration::from_secs(30),
    )?;
    if chmod.code != 0 {
        bail!("making agent executable failed: {}", chmod.stderr);
    }
    // Verifier files are installed only after the agent finishes.

    let instruction_file = dir.join("instruction.md");
    std::fs::write(&instruction_file, &task.instruction)?;
    docker::copy_in(container, &instruction_file, "/logs/agent/instruction.md")?;

    let timeout_secs = task
        .agent_timeout_secs
        .saturating_sub(60)
        .max(60)
        .to_string();
    let max_turns = spec.max_turns.to_string();
    let mut env: Vec<(&str, &str)> = vec![
        ("RIGCODER_PROVIDER", spec.provider),
        ("RIGCODER_MODEL", spec.model),
        ("RUST_LOG", "warn"),
    ];
    if spec.budget.is_some() {
        env.push(("RIGCODER_GEMINI_GATEWAY", "http://127.0.0.1:18080"));
        env.push(("RIGCODER_GATEWAY_TOKEN", "task-relay"));
    } else if let Some((key, value)) = spec.api_key {
        env.push((key, value));
    }
    let checkpoint = if spec.checkpoint {
        " --checkpoint /logs/agent/scenes --checkpoint-tar"
    } else {
        ""
    };
    let mut relay = spec
        .budget
        .map(|budget| {
            crate::trial_relay::Relay::start(
                container,
                budget,
                spec.budget_phase,
                spec.api_key.context("Gemini key is required")?.1,
                task.agent_timeout_secs,
                dir,
            )
        })
        .transpose()?;
    let agent = docker::exec(
        container,
        &task.workdir,
        &env,
        &format!(
            "{REMOTE_BIN} --cwd {} --max-turns {max_turns} --timeout-secs {timeout_secs} \
             --transcript /logs/agent/transcript.jsonl --effect-log /logs/agent/effects.json --observations /logs/agent/observations.json --task-file /logs/agent/instruction.md{checkpoint} \
             > /logs/agent/rigcoder.txt 2>&1; status=$?; echo $status > /logs/agent/exit_code.txt; exit $status",
            shell_quote(&task.workdir)
        ),
        Duration::from_secs(task.agent_timeout_secs),
    );
    let relay_result = relay.as_mut().map_or(Ok(()), |relay| relay.stop());
    // Retain available diagnostics even when transport shutdown invalidates scoring.
    let copied = docker::copy_out(container, "/logs/agent/.", &dir.join("agent"));
    relay_result?;
    copied?;
    let agent = agent?;
    if agent.code == 137 {
        eprintln!(
            "[{}#{}] agent hit the hard timeout",
            task.name, spec.attempt
        );
    }

    if !matches!(agent.code, 0 | 1 | 137) {
        bail!(
            "agent launcher failed (exit {}): {}",
            agent.code,
            agent.stderr
        );
    }
    verify(task, container, dir)
}

fn verify_output_line(
    task: &Task,
    oracle: &crate::task::OutputLine,
    container: &str,
    dir: &Path,
) -> Result<f64> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    // Embed the host scorer: candidate files cannot replace imported modules.
    // Python isolated mode also ignores user site packages and PYTHONPATH.
    let modules = [
        (
            "artifact_score",
            include_str!("../../../harness/artifact_score.py"),
        ),
        (
            "artifact_capture",
            include_str!("../../../harness/artifact_capture.py"),
        ),
    ];
    let mut script = String::from("import sys, types\n");
    for (name, source) in modules {
        script.push_str(&format!(
            "m = types.ModuleType({name:?}); sys.modules[{name:?}] = m\nexec({}, m.__dict__)\n",
            serde_json::to_string(source)?
        ));
    }
    script.push_str(include_str!("../../../harness/artifact_evaluate.py"));
    let mut child = Command::new("python3")
        .args(["-I", "-c", &script])
        .arg(container)
        .arg(dir.join("verifier"))
        .arg(task.verifier_timeout_secs.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting output-line scorer")?;
    child
        .stdin
        .take()
        .context("scorer stdin")?
        .write_all(&serde_json::to_vec(oracle)?)?;
    let output = child.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "output-line scorer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    parse_reward(std::str::from_utf8(&output.stdout)?)
}

/// Install the verifier after the agent stops, discard agent-written reward
/// files, and require a successful verifier invocation with a valid reward.
/// The container is a benchmark environment, not a security sandbox for root.
pub(crate) fn verify(task: &Task, container: &str, dir: &Path) -> Result<f64> {
    if let Some(oracle) = &task.output_line {
        return verify_output_line(task, oracle, container, dir);
    }
    let setup = docker::exec(
        container,
        "/",
        &[],
        "rm -rf /tests /logs/verifier && mkdir -p /logs/verifier",
        Duration::from_secs(30),
    )?;
    if setup.code != 0 {
        bail!("preparing verifier failed: {}", setup.stderr);
    }
    // /tests does not exist, so docker cp does not add an extra nesting level.
    docker::copy_in(container, &task.dir.join("tests"), "/tests")?;
    let verifier = docker::exec(
        container,
        &task.workdir,
        &[],
        "bash /tests/test.sh > /logs/verifier/output.txt 2>&1; status=$?; echo $status > /logs/verifier/exit_code.txt; exit $status",
        Duration::from_secs(task.verifier_timeout_secs),
    )?;
    docker::copy_out(container, "/logs/verifier/.", &dir.join("verifier"))?;
    if verifier.code != 0 {
        bail!(
            "verifier failed (exit {}): {}",
            verifier.code,
            verifier.stderr
        );
    }
    let reward = docker::read_file(container, "/logs/verifier/reward.txt")?
        .with_context(|| "verifier wrote no reward.txt")?;
    parse_reward(&reward)
}

pub(crate) fn parse_reward(text: &str) -> Result<f64> {
    let reward = text
        .trim()
        .parse::<f64>()
        .with_context(|| format!("reward.txt is not a number: {text:?}"))?;
    if !reward.is_finite() || !(0.0..=1.0).contains(&reward) {
        bail!("reward must be finite and between 0 and 1: {text:?}");
    }
    Ok(reward)
}

pub(crate) fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// Tokens, tool calls and the ending, from the agent's transcript.
fn read_agent_output(dir: &Path, task: &str, attempt: usize) -> TrialRecord {
    let mut record = TrialRecord {
        task: task.to_owned(),
        attempt,
        reward: 0.0,
        input_tokens: Some(0),
        output_tokens: Some(0),
        cache_tokens: Some(0),
        tool_calls: 0,
        wall_seconds: 0.0,
        settled: false,
        error: None,
    };
    let Ok(text) = std::fs::read_to_string(dir.join("agent").join("transcript.jsonl")) else {
        record.input_tokens = None;
        record.output_tokens = None;
        record.cache_tokens = None;
        return record;
    };
    let mut saw_usage = false;
    for line in text.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            record.input_tokens = None;
            record.output_tokens = None;
            record.cache_tokens = None;
            continue;
        };
        match event.get("kind").and_then(|k| k.as_str()) {
            Some("tool_call") => record.tool_calls += 1,
            Some("settled") => record.settled = true,
            Some("usage") => {
                saw_usage = true;
                record.input_tokens = add_usage(record.input_tokens, &event["input_tokens"]);
                record.output_tokens = add_usage(record.output_tokens, &event["output_tokens"]);
                record.cache_tokens = add_usage(record.cache_tokens, &event["cached_input_tokens"]);
            }
            _ => {}
        }
    }
    if !saw_usage {
        record.input_tokens = None;
        record.output_tokens = None;
        record.cache_tokens = None;
    }
    record
}

fn add_usage(total: Option<u64>, value: &serde_json::Value) -> Option<u64> {
    total?.checked_add(value.as_u64()?)
}

/// Every trial record under a job directory.
pub fn read_job(job_dir: &Path) -> Result<Vec<TrialRecord>> {
    let mut records = Vec::new();
    for entry in
        std::fs::read_dir(job_dir).with_context(|| format!("reading {}", job_dir.display()))?
    {
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

/// Resume a recorded trial from one of its checkpoints, `times` times, in
/// fresh containers whose workspace is restored from the checkpoint's
/// tarball; returns how many resumed runs the verifier passed.
#[allow(clippy::too_many_arguments)] // All immutable replay inputs belong to one trial.
pub fn branch_from(
    task: &Task,
    trial_dir: &Path,
    turn: usize,
    times: usize,
    binary: &Path,
    provider: &str,
    model: &str,
    api_key: Option<(&str, &str)>,
    max_turns: usize,
    out_dir: &Path,
) -> Result<usize> {
    let scenes = trial_dir.join("agent").join("scenes");
    let scene = scenes.join(format!("turn-{turn:03}.scene.json"));
    let tar = scenes.join(format!("turn-{turn:03}.tar"));
    if !scene.is_file() || !tar.is_file() {
        bail!(
            "no checkpoint {turn} under {}: run the trial with --checkpoint",
            scenes.display()
        );
    }
    anyhow::ensure!(times > 0, "branch-from requires at least one attempt");
    let restore = workspace_restore_script(&task.workdir)?;
    let image = build_image(task, &out_dir.join("image.id"))?;
    let mut passed = 0;
    let mut errors = Vec::new();
    for attempt in 1..=times {
        let dir = out_dir.join(format!("{}__branch{turn}-{attempt}", task.name));
        std::fs::create_dir(&dir)?;
        std::fs::create_dir(dir.join("agent"))?;
        std::fs::create_dir(dir.join("verifier"))?;
        let container = format!(
            "rigcoder-branch-{}-{turn}-{attempt}-{}",
            task.name,
            std::process::id()
        );
        let result = (|| -> Result<f64> {
            docker::start(&container, &image, &task.workdir, task.cpus, &task.memory)?;
            exec_checked(
                &container,
                "/",
                "mkdir -p /logs/agent /restore",
                Duration::from_secs(60),
            )?;
            // Restore before installing the current executable. Never clear a
            // system directory or follow a workspace symlink during restoration.
            docker::copy_in(&container, &tar, "/restore/workspace.tar")?;
            exec_checked(&container, "/", &restore, Duration::from_secs(120))?;
            docker::copy_in(&container, binary, REMOTE_BIN)?;
            exec_checked(
                &container,
                "/",
                &format!("chmod 755 {REMOTE_BIN}"),
                Duration::from_secs(30),
            )?;
            docker::copy_in(&container, &scene, "/logs/agent/resume.scene.json")?;
            let timeout_secs = task
                .agent_timeout_secs
                .saturating_sub(60)
                .max(60)
                .to_string();
            let max_turns = max_turns.to_string();
            let mut env: Vec<(&str, &str)> = vec![
                ("RIGCODER_PROVIDER", provider),
                ("RIGCODER_MODEL", model),
                ("RUST_LOG", "warn"),
            ];
            if let Some((key, value)) = api_key {
                env.push((key, value));
            }
            let agent = docker::exec(
                &container,
                &task.workdir,
                &env,
                &format!(
                    "{REMOTE_BIN} --cwd {} --max-turns {max_turns} --timeout-secs {timeout_secs} \
                     --transcript /logs/agent/transcript.jsonl --effect-log /logs/agent/effects.json --observations /logs/agent/observations.json \
                     --resume /logs/agent/resume.scene.json > /logs/agent/rigcoder.txt 2>&1; status=$?; echo $status > /logs/agent/exit_code.txt; exit $status",
                    shell_quote(&task.workdir)
                ),
                Duration::from_secs(task.agent_timeout_secs),
            )?;
            docker::copy_out(&container, "/logs/agent/.", &dir.join("agent"))?;
            if !matches!(agent.code, 0 | 1 | 137) {
                bail!(
                    "resumed agent launcher failed (exit {}): {}",
                    agent.code,
                    agent.stderr
                );
            }
            verify(task, &container, &dir)
        })();
        docker::remove(&container);
        match result {
            Ok(reward) => {
                println!("branch from turn {turn}, attempt {attempt}: reward {reward:.1}");
                if reward >= 1.0 {
                    passed += 1;
                }
            }
            Err(error) => {
                let error = format!("branch from turn {turn}, attempt {attempt}: {error:#}");
                eprintln!("{error}");
                errors.push(error);
            }
        }
    }
    anyhow::ensure!(
        errors.is_empty(),
        "incomplete branch evaluation: {}",
        errors.join("; ")
    );
    Ok(passed)
}

fn exec_checked(container: &str, workdir: &str, script: &str, timeout: Duration) -> Result<()> {
    let output = docker::exec(container, workdir, &[], script, timeout)?;
    anyhow::ensure!(
        output.code == 0,
        "container command failed (exit {}): {}",
        output.code,
        last_lines(&output.stderr, 20)
    );
    Ok(())
}

fn workspace_restore_script(workdir: &str) -> Result<String> {
    use std::path::Component;
    let path = Path::new(workdir);
    anyhow::ensure!(
        path.is_absolute()
            && path != Path::new("/")
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "checkpoint workspace must be an absolute directory below / without traversal"
    );
    for reserved in [
        "/bin", "/sbin", "/usr", "/etc", "/lib", "/lib64", "/dev", "/proc", "/sys", "/run",
        "/logs", "/tests", "/restore",
    ] {
        anyhow::ensure!(
            !path.starts_with(reserved),
            "cannot restore a checkpoint over {reserved}"
        );
    }
    anyhow::ensure!(workdir != "/var", "cannot restore a checkpoint over /var");
    let quoted = shell_quote(workdir.trim_end_matches('/'));
    Ok(format!(
        "set -eu; w={quoted}; [ \"$(cd -- \"$w\" && pwd -P)\" = \"$w\" ]; find \"$w\" -mindepth 1 -delete; tar -xf /restore/workspace.tar -C \"$w\""
    ))
}

#[cfg(test)]
mod branch_tests {
    use super::*;

    #[test]
    fn restoration_refuses_system_roots_and_traversal() {
        for path in [
            "/", "/app/../", "/usr", "/logs", "/restore", "/tests", "app", "/var",
        ] {
            assert!(workspace_restore_script(path).is_err(), "{path}");
        }
        assert!(workspace_restore_script("/app").is_ok());
    }

    #[test]
    fn workspace_shell_arguments_are_quoted() {
        assert_eq!(shell_quote("/app/a b'c"), "'/app/a b'\"'\"'c'");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_parsing_preserves_zero_but_not_missing_or_corrupt_measurements() {
        let root = std::env::temp_dir().join(format!(
            "rigcoder-usage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("agent")).unwrap();
        let zero = r#"{"kind":"usage","input_tokens":0,"output_tokens":0,"cached_input_tokens":0}"#;
        let partial = r#"{"kind":"usage","input_tokens":3,"output_tokens":2}"#;
        for (text, expected) in [
            (zero.to_owned(), (Some(0), Some(0), Some(0))),
            (format!("{partial}\n{zero}"), (Some(3), Some(2), None)),
            (format!("broken\n{zero}"), (None, None, None)),
            (format!("{zero}\nbroken"), (None, None, None)),
            (r#"{"kind":"settled"}"#.to_owned(), (None, None, None)),
        ] {
            std::fs::write(root.join("agent/transcript.jsonl"), &text).unwrap();
            let record = read_agent_output(&root, "task", 1);
            assert_eq!(
                (
                    record.input_tokens,
                    record.output_tokens,
                    record.cache_tokens
                ),
                expected,
                "{text}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rewards_are_finite_probabilities() {
        for text in ["NaN", "inf", "-inf", "1.1", "-0.1", "invalid"] {
            assert!(parse_reward(text).is_err(), "accepted {text}");
        }
        assert_eq!(parse_reward(" 0.5\n").unwrap(), 0.5);
        assert_eq!(parse_reward("0").unwrap(), 0.0);
        assert_eq!(parse_reward("1").unwrap(), 1.0);
    }

    #[test]
    fn workdir_quotes_preserve_spaces_and_apostrophes() {
        let path = "/app/a b'c;$(touch nope)";
        let out = std::process::Command::new("bash")
            .args(["-c", &format!("printf %s {}", shell_quote(path))])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), path);
    }
}
