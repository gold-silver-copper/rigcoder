//! A Terminal-Bench task directory.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    pub dir: PathBuf,
    pub instruction: String,
    /// The Dockerfile's last `WORKDIR`; the agent and the verifier run there.
    pub workdir: String,
    pub agent_timeout_secs: u64,
    pub verifier_timeout_secs: u64,
    pub build_timeout_secs: u64,
    pub cpus: f64,
    pub memory: String,
    pub difficulty: String,
    pub output_line: Option<OutputLine>,
    pub polyglot: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputLine {
    pub artifact: String,
    pub expected: String,
}

#[derive(Deserialize, Default)]
struct TaskToml {
    #[serde(default)]
    metadata: Metadata,
    #[serde(default)]
    agent: Timeout,
    #[serde(default)]
    verifier: Timeout,
    #[serde(default)]
    environment: Environment,
}

#[derive(Deserialize, Default)]
struct Metadata {
    #[serde(default)]
    difficulty: String,
}

#[derive(Deserialize)]
struct Timeout {
    #[serde(default = "default_timeout")]
    timeout_sec: f64,
}

impl Default for Timeout {
    fn default() -> Self {
        Self {
            timeout_sec: default_timeout(),
        }
    }
}

fn default_timeout() -> f64 {
    900.0
}

#[derive(Deserialize)]
struct Environment {
    #[serde(default = "default_build_timeout")]
    build_timeout_sec: f64,
    #[serde(default = "default_cpus")]
    cpus: f64,
    #[serde(default = "default_memory")]
    memory: String,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            build_timeout_sec: default_build_timeout(),
            cpus: default_cpus(),
            memory: default_memory(),
        }
    }
}

fn default_build_timeout() -> f64 {
    600.0
}
fn default_cpus() -> f64 {
    1.0
}
fn default_memory() -> String {
    "2G".to_owned()
}

impl Task {
    pub fn load(tasks_dir: &Path, name: &str) -> Result<Self> {
        let dir = tasks_dir.join(name);
        let toml_text = std::fs::read_to_string(dir.join("task.toml"))
            .with_context(|| format!("no task.toml under {}", dir.display()))?;
        let parsed: TaskToml =
            toml::from_str(&toml_text).with_context(|| format!("{name}/task.toml"))?;
        let instruction = std::fs::read_to_string(dir.join("instruction.md"))
            .with_context(|| format!("{name}/instruction.md"))?;
        let dockerfile = std::fs::read_to_string(dir.join("environment").join("Dockerfile"))
            .with_context(|| format!("{name}/environment/Dockerfile"))?;
        let workdir = dockerfile
            .lines()
            .filter_map(|line| line.trim().strip_prefix("WORKDIR"))
            .map(|rest| rest.trim().to_owned())
            .next_back()
            .unwrap_or_else(|| "/app".to_owned());
        let oracle_path = dir.join("tests/output-line.json");
        let output_line = match std::fs::symlink_metadata(&oracle_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
            Ok(meta) => {
                anyhow::ensure!(
                    meta.is_file() && meta.len() <= 70_000,
                    "invalid output-line oracle file"
                );
                let oracle: OutputLine = serde_json::from_slice(&std::fs::read(&oracle_path)?)?;
                anyhow::ensure!(
                    oracle.artifact.starts_with('/')
                        && oracle.artifact.len() <= 4096
                        && !oracle.artifact.contains('\0')
                        && !oracle.expected.is_empty()
                        && oracle.expected.len() <= 65_536
                        && !oracle.expected.contains(['\n', '\r']),
                    "invalid output-line oracle"
                );
                Some(oracle)
            }
        };
        let polyglot_path = dir.join("tests/polyglot.json");
        let polyglot = match std::fs::symlink_metadata(&polyglot_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
            Ok(meta) => {
                anyhow::ensure!(
                    meta.is_file() && meta.len() <= 128,
                    "invalid polyglot scorer marker"
                );
                let marker: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&polyglot_path)?)?;
                anyhow::ensure!(
                    marker == serde_json::json!({"version": 1}) && output_line.is_none(),
                    "invalid or conflicting polyglot scorer marker"
                );
                true
            }
        };
        Ok(Self {
            name: name.to_owned(),
            dir,
            instruction,
            workdir,
            agent_timeout_secs: timeout_seconds(parsed.agent.timeout_sec, "agent.timeout_sec")?,
            verifier_timeout_secs: timeout_seconds(
                parsed.verifier.timeout_sec,
                "verifier.timeout_sec",
            )?,
            build_timeout_secs: timeout_seconds(
                parsed.environment.build_timeout_sec,
                "environment.build_timeout_sec",
            )?,
            cpus: parsed.environment.cpus,
            memory: parsed.environment.memory,
            difficulty: parsed.metadata.difficulty,
            output_line,
            polyglot,
        })
    }

    pub fn image_tag(&self) -> String {
        format!("rigcoder-bench/{}:local", self.name)
    }
}

// Execution uses whole-second GNU timeout arguments; zero disables the
// deadline. Reject values that cannot produce a positive bounded duration.
fn timeout_seconds(value: f64, field: &str) -> Result<u64> {
    let duration = Duration::try_from_secs_f64(value)
        .with_context(|| format!("{field} must be a finite positive timeout"))?;
    let seconds = duration.as_secs();
    anyhow::ensure!(seconds > 0, "{field} must be at least one second");
    Ok(seconds)
}
