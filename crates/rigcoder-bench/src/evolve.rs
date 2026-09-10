//! `run`: evaluate a slice. `iterate`: the self-improvement loop over it.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::{
    docker, ledger, report, slices,
    stats::{self, Decision, Summary},
    task::Task,
    trial::{self, TrialRecord, TrialSpec},
};

/// Everything any lane may change: the revert set. Everything else is the
/// harness's, not the agent's.
pub const MUTABLE: &[&str] = &[
    "crates/rigcoder/src/prompt.md",
    "crates/rigcoder/src/tools.rs",
    "crates/rigcoder/src/tools/",
    "crates/rigcoder/src/lib.rs",
    "crates/rigcoder-cli/src/main.rs",
    "crates/rigcoder/src/session.rs",
    "crates/rigcoder/src/steer.rs",
];

/// What the meta agent must never read: the held-out tasks, its own scores,
/// and the harness that scores it.
pub const META_FORBIDDEN: &[&str] = &[
    "harness/slices/holdout.txt",
    "harness/ledger.jsonl",
    "crates/rigcoder-bench/",
    "harness/notes/",
];

/// One kind of change per generation, so the ledger can say which kind
/// moved the score.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// The system prompt.
    Prompt,
    /// Tool descriptions and behaviours.
    Tools,
    /// The agent components (max tokens, turns, tool concurrency, temperature) and the CLI defaults.
    Settings,
    /// How tool results and history are shaped before the model sees them.
    Shaping,
    /// Gate and Judge steering systems.
    Systems,
}

impl Lane {
    pub const ALL: [Lane; 5] = [
        Lane::Prompt,
        Lane::Tools,
        Lane::Settings,
        Lane::Shaping,
        Lane::Systems,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Lane::Prompt => "prompt",
            Lane::Tools => "tools",
            Lane::Settings => "settings",
            Lane::Shaping => "shaping",
            Lane::Systems => "systems",
        }
    }

    /// The files the lane may touch.
    pub fn files(self) -> &'static [&'static str] {
        match self {
            Lane::Prompt => &["crates/rigcoder/src/prompt.md"],
            Lane::Tools => &["crates/rigcoder/src/tools.rs", "crates/rigcoder/src/tools/"],
            Lane::Settings => &[
                "crates/rigcoder/src/lib.rs",
                "crates/rigcoder-cli/src/main.rs",
            ],
            Lane::Shaping => &["crates/rigcoder/src/session.rs"],
            Lane::Systems => &["crates/rigcoder/src/steer.rs"],
        }
    }

    fn levers(self) -> &'static str {
        match self {
            Lane::Prompt => {
                "the system prompt: process, verification habits, when to stop, what to do before the final message"
            }
            Lane::Tools => {
                "tool descriptions and behaviours: output limits, timeouts, error messages the model can act on, what a result shows"
            }
            Lane::Settings => {
                "the agent components set in lib.rs::setup (MaxTokens, MaxTurns, ToolPolicy concurrency, Temperature) and the CLI defaults"
            }
            Lane::Shaping => {
                "how tool results and history are shaped before the model sees them (session.rs)"
            }
            Lane::Systems => {
                "Gate and Judge systems in steer.rs: deny or hold dangerous tool calls, rewrite over-long results, retry a turn that would settle with deliverables missing"
            }
        }
    }

    /// Pick the lane the digest points at, or the next in round-robin.
    pub fn choose(digest: Option<&crate::digest::Digest>, previous: Option<Lane>) -> Lane {
        if let Some(d) = digest
            && d.failed.trials > 0
        {
            let f = &d.failed;
            let p = &d.passed;
            if f.timeouts > p.timeouts + 0.25 || f.truncations > p.truncations + 0.25 {
                return Lane::Tools;
            }
            if f.ended_deliberating > p.ended_deliberating + 0.25
                || f.repeated_calls > p.repeated_calls + 1.0
                || f.calls_before_first_edit > p.calls_before_first_edit * 1.5 + 5.0
            {
                return Lane::Prompt;
            }
            if let (Some(failed), Some(passed)) = (f.input_tokens, p.input_tokens)
                && failed > passed * 1.5
                && passed > 0.0
            {
                return Lane::Shaping;
            }
            if f.no_settle > p.no_settle + 0.25 {
                return Lane::Systems;
            }
        }
        let index = previous.map_or(0, |l| {
            (Lane::ALL.iter().position(|x| *x == l).unwrap_or(0) + 1) % Lane::ALL.len()
        });
        Lane::ALL[index]
    }
}

/// Is `path` inside one of `files` (a file, or a directory prefix)?
pub fn covered(files: &[&str], path: &str) -> bool {
    files.iter().any(|f| {
        if f.ends_with('/') {
            path.starts_with(f)
        } else {
            path == *f
        }
    })
}

const META_TASK: &str = "You are improving rigcoder, the coding agent in this repository, so it scores higher on Terminal-Bench.

Read {report} first for the failure digest and evidence (instructions, transcripts, verifier output). Oversized reports link report.full.md; read relevant ranges there when the excerpt omits needed evidence.

This generation works in the {lane} lane: {levers}. You may only edit these files: {mutable}. Nothing else.

Rules:
- Make one coherent improvement aimed at the failure patterns the digest shows, not many unrelated tweaks.
- Do not touch the harness/ directory except the requested note, the rigcoder-bench crate, Cargo.toml files, or the model choice. Do not read {forbidden}.
- Use read_file, list_files and grep to inspect the repository, and write_file/edit_file for changes. Bash is disabled in this scoped improvement run. The trusted harness will run cargo check --workspace after validating the edit; finish with your note once the file changes are ready.
- Finish with a short note: what you changed and which failures it targets. Write that note to {note}.
";

fn default_binary() -> PathBuf {
    PathBuf::from(format!(
        "harness/bin/rigcoder-linux-{}",
        std::env::consts::ARCH
    ))
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Task slice under harness/slices/ (dev or holdout).
    #[arg(long, default_value = "dev")]
    pub slice: String,
    /// Explicit task names instead of the slice (repeatable).
    #[arg(short = 'i', long = "include")]
    pub include: Vec<String>,
    /// Attempts per task.
    #[arg(short = 'k', long, default_value_t = 3)]
    pub attempts: usize,
    /// Trials running at once.
    #[arg(short = 'n', long, default_value_t = 4)]
    pub concurrency: usize,
    /// provider/model the benchmarked agent uses.
    #[arg(short = 'm', long, default_value = "gemini/gemini-3.8-flash")]
    pub model: String,
    /// Existing trusted Gemini ledger; isolates task networking and uses host scoring.
    #[arg(long)]
    pub gemini_budget: Option<PathBuf>,
    /// Model calls per run inside the container.
    #[arg(long, default_value_t = 200)]
    pub max_turns: usize,
    /// Task directories (a checkout of laude-institute/terminal-bench-2).
    #[arg(long, default_value = "harness/tasks")]
    pub tasks_dir: PathBuf,
    /// The Linux rigcoder binary to upload.
    #[arg(long, default_value_os_t = default_binary())]
    pub binary: PathBuf,
    /// Skip harness/build-linux.sh (evaluation only).
    #[arg(long)]
    pub no_build: bool,
    /// Job prefix; each evaluation gets a fresh directory including its label.
    #[arg(long)]
    pub job_name: Option<String>,
    /// Save a scene and a workspace tarball after every turn into <trial>/agent/scenes/.
    #[arg(long)]
    pub checkpoint: bool,
}

#[derive(Args, Debug, Clone)]
pub struct IterateArgs {
    #[command(flatten)]
    pub run: RunArgs,
    #[arg(long, default_value_t = 3)]
    pub generations: usize,
    /// Provider the improvement step uses (its key from the environment).
    #[arg(long, default_value = "gemini")]
    pub meta_provider: String,
    /// Model the improvement step uses (default: the provider's default).
    #[arg(long)]
    pub meta_model: Option<String>,
    /// Use the isolated macOS prompt launcher through this local budget gateway.
    /// Requires --lane prompt and RIGCODER_GATEWAY_TOKEN; affects only self-editing.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub meta_gateway_port: Option<u16>,
    /// Evaluate only; never self-edit.
    #[arg(long)]
    pub no_improve: bool,
    /// Score the best kept generation on the holdout slice at the end.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub holdout: bool,
    /// Force one lane for every generation instead of choosing from the digest.
    #[arg(long, value_enum)]
    pub lane: Option<Lane>,
}

pub fn provider_key(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "gemini" => Some("GEMINI_API_KEY"),
        _ => None,
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root)
        .output()
        .context("git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)
        .context("git output is not UTF-8")?
        .trim_end_matches(['\n', '\r'])
        .to_owned())
}

fn build_linux(root: &Path, binary: &Path) -> Result<()> {
    let arch = match binary.file_name().and_then(|name| name.to_str()) {
        Some("rigcoder-linux-aarch64") => "aarch64",
        Some("rigcoder-linux-x86_64") => "x86_64",
        _ => bail!(
            "building requires a harness/bin/rigcoder-linux-<architecture> binary; use --no-build for custom binaries"
        ),
    };
    if root.join(binary) != root.join(format!("harness/bin/rigcoder-linux-{arch}")) {
        bail!(
            "the build script does not produce {}; use its harness/bin output",
            binary.display()
        );
    }
    let output = root.join(binary);
    remove_build_output(&output)?;
    println!("$ bash harness/build-linux.sh");
    let status = Command::new("bash")
        .arg("harness/build-linux.sh")
        .env("ARCH", arch)
        .current_dir(root)
        .status()?;
    if !status.success() {
        remove_build_output(&output)?;
        bail!("harness/build-linux.sh failed");
    }
    anyhow::ensure!(
        output.is_file(),
        "build succeeded without producing {}",
        output.display()
    );
    Ok(())
}

// A failed build must not leave an earlier or partially produced executable
// available to a subsequent evaluation-only run.
fn remove_build_output(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn validate_run(args: &RunArgs, tasks: &[String]) -> Result<()> {
    if args.gemini_budget.is_some() {
        anyhow::ensure!(
            matches!(
                args.model.as_str(),
                "gemini/gemini-3.8-flash" | "gemini-3.8-flash"
            ) && !args.checkpoint,
            "--gemini-budget requires Gemini gemini-3.8-flash without checkpoints"
        );
    }
    if tasks.is_empty() || args.attempts == 0 || args.concurrency == 0 {
        bail!("evaluation needs at least one task, attempt, and worker");
    }
    if tasks
        .iter()
        .any(|name| name.is_empty() || name.contains(['/', '\\']) || name == "." || name == "..")
    {
        bail!("task names must be single directory names");
    }
    if tasks.iter().collect::<BTreeSet<_>>().len() != tasks.len() {
        bail!("task names must be unique");
    }
    Ok(())
}

fn new_job_dir(root: &Path, prefix: Option<&str>, label: &str) -> Result<PathBuf> {
    if label.contains(['/', '\\']) {
        bail!("job labels must not contain directory separators");
    }
    if prefix.is_some_and(|name| {
        name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".."
    }) {
        bail!("job name must be a single directory name");
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let name = format!(
        "{}{}{}-{stamp}-{}",
        prefix.unwrap_or(""),
        if prefix.is_some() { "-" } else { "" },
        label,
        std::process::id()
    );
    let runs = root.join("harness/runs");
    std::fs::create_dir_all(&runs)?;
    let dir = runs.join(name);
    // Never reuse a directory: otherwise stale transcripts and rewards look current.
    std::fs::create_dir(&dir).with_context(|| format!("creating fresh job {}", dir.display()))?;
    Ok(dir)
}

// Keep mutable dataset paths out of trial execution. Refuse links and special
// files rather than accidentally including inputs outside the declared task.
fn snapshot_inputs(source: &Path, target: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            snapshot_inputs(&entry.path(), &target.join(entry.file_name()))?;
        }
        std::fs::set_permissions(target, metadata.permissions())?;
    } else if metadata.is_file() {
        std::fs::copy(source, target)?;
    } else {
        bail!(
            "task snapshot requires regular files and directories: {}",
            source.display()
        );
    }
    Ok(())
}

fn input_identity(root: &Path, directory: &Path) -> Result<BTreeMap<String, serde_json::Value>> {
    fn visit(
        root: &Path,
        base: &Path,
        directory: &Path,
        files: &mut BTreeMap<String, serde_json::Value>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            let identity = if metadata.is_dir() {
                visit(root, base, &path, files)?;
                serde_json::json!({"kind": "directory"})
            } else {
                anyhow::ensure!(metadata.is_file(), "non-file snapshot input");
                let path_text = path.to_str().context("task path is not UTF-8")?;
                serde_json::json!({
                    "kind": "file",
                    "git_blob_oid": git(root, &["hash-object", "--no-filters", "--", path_text])?,
                })
            };
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::PermissionsExt;
                let mut identity = identity;
                identity["mode"] = metadata.permissions().mode().into();
                identity
            };
            files.insert(
                path.strip_prefix(base)?
                    .to_str()
                    .context("task path is not UTF-8")?
                    .to_owned(),
                identity,
            );
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, directory, directory, &mut files)?;
    Ok(files)
}

/// Evaluate frozen task inputs with `attempts` each, `concurrency` at a time.
pub fn evaluate(
    root: &Path,
    args: &RunArgs,
    label: &str,
    tasks: &[String],
    budget_phase: &str,
) -> Result<(Vec<TrialRecord>, PathBuf)> {
    validate_run(args, tasks)?;
    docker::available()?;
    let job_dir = new_job_dir(root, args.job_name.as_deref(), label)?;
    let job = job_dir.file_name().unwrap().to_string_lossy();
    let (provider, model) = args
        .model
        .split_once('/')
        .unwrap_or(("gemini", &args.model));
    let key_name = provider_key(provider);
    let key_value = key_name.and_then(|k| std::env::var(k).ok());
    if key_name.is_some() && key_value.is_none() {
        bail!("{} is not set", key_name.unwrap_or("the provider key"));
    }
    let binary = root.join(&args.binary);
    if !binary.is_file() {
        bail!(
            "{} is missing; run harness/build-linux.sh",
            binary.display()
        );
    }
    let tasks_dir = root.join(&args.tasks_dir);
    let snapshot_dir = job_dir.join("tasks");
    for name in tasks {
        let destination = snapshot_dir.join(name);
        std::fs::create_dir_all(&destination)?;
        for input in ["task.toml", "instruction.md", "environment", "tests"] {
            snapshot_inputs(&tasks_dir.join(name).join(input), &destination.join(input))?;
        }
    }
    let loaded: Vec<Task> = tasks
        .iter()
        .map(|name| Task::load(&snapshot_dir, name))
        .collect::<Result<_>>()?;
    let budget = args
        .gemini_budget
        .as_ref()
        .map(|path| root.join(path).canonicalize())
        .transpose()?;
    if budget.is_some() {
        anyhow::ensure!(
            loaded.iter().all(|task| (task.output_line.is_some()
                || task.polyglot
                || task.vim_macros.is_some())
                && task.agent_timeout_secs <= 7200),
            "budgeted trials require trusted host scoring and timeouts at most 7200 seconds"
        );
    }
    // A build or external replacement between trials must not mix executable
    // versions within one score. Every upload uses this job-owned snapshot.
    let original_binary = binary;
    let binary = job_dir.join("rigcoder.bin");
    std::fs::copy(&original_binary, &binary).context("snapshotting evaluated binary")?;
    // Docker preserves copied file ownership. Set the private snapshot's mode
    // here so capability-free containers need not chmod a host-owned file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))?;
    }
    let receipt_path = original_binary.with_file_name(format!(
        "{}.build.json",
        original_binary
            .file_name()
            .and_then(|name| name.to_str())
            .context("binary filename is not UTF-8")?
    ));
    let has_receipt = match std::fs::symlink_metadata(&receipt_path) {
        Ok(metadata) => {
            anyhow::ensure!(metadata.is_file(), "build receipt must be a regular file");
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("inspecting build receipt"),
    };
    let receipt = if has_receipt {
        let arch = match original_binary.file_name().and_then(|name| name.to_str()) {
            Some("rigcoder-linux-x86_64") => "x86_64",
            Some("rigcoder-linux-aarch64") => "aarch64",
            _ => std::env::consts::ARCH,
        };
        let checked = Command::new("python3")
            .arg(root.join("harness/build-inputs.py"))
            .arg("verify")
            .arg(root)
            .arg(&binary)
            .arg(&receipt_path)
            .arg(arch)
            .output()
            .context("validating build receipt")?;
        anyhow::ensure!(
            checked.status.success(),
            "build receipt validation failed: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
        Some(
            serde_json::from_slice::<serde_json::Value>(&checked.stdout)
                .context("reading validated receipt")?,
        )
    } else {
        anyhow::ensure!(args.no_build, "normal evaluation requires a build receipt");
        None
    };
    let binary_path = binary.to_str().context("binary path is not UTF-8")?;
    let mut manifest = serde_json::json!({
        "version": 1,
        "source_head": git(root, &["rev-parse", "HEAD"])?,
        "source_dirty": !git(root, &["status", "--porcelain"])?.is_empty(),
        "binary": {
            "file": "rigcoder.bin",
            "git_object_format": git(root, &["rev-parse", "--show-object-format"])?,
            "git_blob_oid": git(root, &["hash-object", "--no-filters", "--", binary_path])?,
            "source_binding": if receipt.is_some() { "matched_receipt" } else { "unverified" },
            "build_receipt": receipt,
        },
        "provider": provider,
        "model": model,
        "provider_transport": if budget.is_some() { "host_budgeted_pipe_relay" } else { "direct" },
        "budget_ledger": budget,
        "budget_phase": budget_phase,
        "tasks": tasks,
        "task_files": input_identity(root, &snapshot_dir)?,
        "attempts": args.attempts,
        "concurrency": args.concurrency,
        "max_turns": args.max_turns,
        "checkpoint": args.checkpoint,
    });
    std::fs::write(
        job_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    println!(
        "job {job}: {} task(s) × {} attempt(s), {} at a time, {}",
        loaded.len(),
        args.attempts,
        args.concurrency,
        args.model
    );

    // Images first, in parallel: docker serializes what it must.
    let build_errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let images = Mutex::new(BTreeMap::new());
    let queue: Mutex<VecDeque<&Task>> = Mutex::new(loaded.iter().collect());
    std::thread::scope(|scope| {
        for _ in 0..args.concurrency.max(1) {
            scope.spawn(|| {
                loop {
                    let Some(task) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    println!(
                        "building {} ({}, {}s agent timeout)",
                        task.name, task.difficulty, task.agent_timeout_secs
                    );
                    match trial::build_image(
                        task,
                        &job_dir.join("images").join(format!("{}.id", task.name)),
                    ) {
                        Ok(image) => {
                            images.lock().unwrap().insert(task.name.clone(), image);
                        }
                        Err(error) => build_errors.lock().unwrap().push(format!("{error:#}")),
                    }
                }
            });
        }
    });
    let images = images.into_inner().unwrap();
    manifest["images"] = serde_json::to_value(&images)?;
    std::fs::write(
        job_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    let build_errors = build_errors.into_inner().unwrap();
    if !build_errors.is_empty() {
        let errors = build_errors.join("\n");
        std::fs::write(job_dir.join("error.txt"), &errors)?;
        bail!("evaluation incomplete: image builds failed:\n{errors}");
    }

    let mut specs: Vec<(usize, &Task)> = Vec::new();
    for attempt in 1..=args.attempts {
        for task in &loaded {
            specs.push((attempt, task));
        }
    }
    let results: Mutex<Vec<TrialRecord>> = Mutex::new(Vec::new());
    let total = specs.len();
    let queue: Mutex<VecDeque<(usize, &Task)>> = Mutex::new(specs.into_iter().collect());
    std::thread::scope(|scope| {
        for _ in 0..args.concurrency.max(1) {
            scope.spawn(|| {
                loop {
                    let Some((attempt, task)) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    let spec = TrialSpec {
                        task,
                        image: &images[&task.name],
                        attempt,
                        job_dir: &job_dir,
                        binary: &binary,
                        provider,
                        model,
                        api_key: key_name.zip(key_value.as_deref()),
                        budget: budget.as_deref(),
                        budget_phase,
                        max_turns: args.max_turns,
                        checkpoint: args.checkpoint,
                    };
                    let record = trial::run(&spec);
                    let mut results = results.lock().unwrap();
                    results.push(record.clone());
                    println!(
                        "[{}/{}] {}#{} reward {:.1} {}s {} calls{}",
                        results.len(),
                        total,
                        record.task,
                        record.attempt,
                        record.reward,
                        record.wall_seconds as u64,
                        record.tool_calls,
                        record
                            .error
                            .as_ref()
                            .map_or(String::new(), |e| format!(" ERROR {e}"))
                    );
                }
            });
        }
    });
    let mut records = results.into_inner().unwrap();
    records.sort_by(|a, b| (&a.task, a.attempt).cmp(&(&b.task, b.attempt)));
    let errors: Vec<String> = records
        .iter()
        .filter_map(|record| {
            record
                .error
                .as_ref()
                .map(|error| format!("{}#{}: {error}", record.task, record.attempt))
        })
        .collect();
    if !errors.is_empty() {
        let errors = errors.join("\n");
        std::fs::write(job_dir.join("error.txt"), &errors)?;
        bail!("evaluation incomplete: trial infrastructure failed:\n{errors}");
    }
    let summary = stats::summarize(&records);
    std::fs::write(
        job_dir.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    println!(
        "job {job}: score {:.3} [{:.3}, {:.3}] pass@1 {:.3} pass@k {:.3} over {} trials",
        summary.score,
        summary.ci_low,
        summary.ci_high,
        summary.pass1,
        summary.passk,
        summary.trials
    );
    Ok((records, job_dir))
}

fn slice_name(args: &RunArgs) -> String {
    if args.include.is_empty() {
        args.slice.clone()
    } else {
        "custom".to_owned()
    }
}

fn tasks_for(root: &Path, args: &RunArgs) -> Result<Vec<String>> {
    if args.include.is_empty() {
        slices::read(root, &args.slice)
    } else {
        Ok(args.include.clone())
    }
}

pub fn run(root: &Path, args: RunArgs) -> Result<()> {
    require_no_pending_improvement(root)?;
    let tasks = tasks_for(root, &args)?;
    validate_run(&args, &tasks)?;
    if !args.no_build {
        build_linux(root, &args.binary)?;
    }
    let (records, job_dir) = evaluate(
        root,
        &args,
        &format!("run-{}", slice_name(&args)),
        &tasks,
        if args.slice == "holdout" {
            "holdout"
        } else {
            "development"
        },
    )?;
    let summary = stats::summarize(&records);
    ledger::append(
        root,
        &entry(
            root,
            None,
            &slice_name(&args),
            None,
            (-1.0, -1.0),
            &args,
            &job_dir,
            summary,
        )?,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)] // The ledger schema is assembled in one place.
fn entry(
    root: &Path,
    generation: Option<usize>,
    slice: &str,
    decision: Option<Decision>,
    best: (f64, f64),
    args: &RunArgs,
    job_dir: &Path,
    summary: Summary,
) -> Result<ledger::Entry> {
    Ok(ledger::Entry {
        generation,
        slice: slice.to_owned(),
        commit: git(root, &["rev-parse", "--short", "HEAD"])?,
        decision,
        best_score: best.0,
        best_ci_low: best.1,
        model: args.model.clone(),
        attempts: args.attempts,
        lane: None,
        meta_commit: None,
        job_dir: job_dir.display().to_string(),
        time: now(),
        summary,
    })
}

fn pending_improvement(root: &Path) -> Result<PathBuf> {
    Ok(root.join(git(
        root,
        &[
            "rev-parse",
            "--git-path",
            "rigcoder-pending-improvement.json",
        ],
    )?))
}

fn require_no_pending_improvement(root: &Path) -> Result<()> {
    let path = pending_improvement(root)?;
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => bail!(
            "unfinished self-improvement recorded at {}; inspect and recover source, index and binary before removing this marker",
            path.display()
        ),
    }
}

fn begin_improvement(root: &Path, binary: &Path, note: &Path, head: &str) -> Result<()> {
    use std::io::Write;
    let path = pending_improvement(root)?;
    let mut file = std::fs::File::create_new(&path)?;
    let record = serde_json::json!({"baseline": head, "binary": binary, "note": note});
    file.write_all(serde_json::to_string_pretty(&record)?.as_bytes())?;
    file.sync_all()?;
    std::fs::File::open(path.parent().context("marker parent")?)?.sync_all()?;
    Ok(())
}

fn finish_improvement(root: &Path) -> Result<()> {
    let path = pending_improvement(root)?;
    match std::fs::remove_file(&path) {
        Ok(()) => std::fs::File::open(path.parent().context("marker parent")?)?
            .sync_all()
            .map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn improve(
    root: &Path,
    args: &IterateArgs,
    report_path: &Path,
    lane: Lane,
    generation: usize,
) -> Result<PathBuf> {
    let notes_dir = root.join("harness").join("notes");
    std::fs::create_dir_all(&notes_dir)?;
    let job_name = report_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .context("report job directory name")?;
    let note = notes_dir.join(format!("gen-{generation:03}-{}-{job_name}.md", lane.name()));
    if let Some(port) = args.meta_gateway_port {
        return improve_isolated_prompt(root, args, report_path, &note, port);
    }
    let task = META_TASK
        .replace("{report}", &report_path.display().to_string())
        .replace("{lane}", lane.name())
        .replace("{levers}", lane.levers())
        .replace("{mutable}", &lane.files().join(", "))
        .replace("{forbidden}", &META_FORBIDDEN.join(", "))
        .replace("{note}", &note.display().to_string());
    let host_bin = host_binary(root)?;
    println!(
        "$ {} --cwd {} (meta task, {} chars, lane {})",
        host_bin.display(),
        root.display(),
        task.len(),
        lane.name()
    );
    let mut cmd = Command::new(&host_bin);
    cmd.arg("--cwd")
        .arg(root)
        .args([
            "--max-turns",
            "80",
            "--timeout-secs",
            "1800",
            "--transcript",
        ])
        .arg(report_path.with_file_name("improve-transcript.jsonl"))
        .arg("--effect-log")
        .arg(report_path.with_file_name("improve-effects.json"))
        .arg("--checkpoint")
        .arg(report_path.with_file_name("improve-scenes"));
    // Scoped file tools enforce the lane and Bash is disabled. Validate all
    // tracked, staged and untracked state before the trusted build.
    for file in lane.files() {
        cmd.arg("--allow").arg(file);
    }
    cmd.arg("--allow").arg(&note);
    for denied in [
        "harness/",
        "Cargo.toml",
        "Cargo.lock",
        "crates/rigcoder-bench/",
        ".github/",
    ] {
        cmd.arg("--deny-path").arg(denied);
    }
    cmd.arg(&task)
        .env("RIGCODER_PROVIDER", &args.meta_provider)
        .current_dir(root);
    if let Some(model) = &args.meta_model {
        cmd.env("RIGCODER_MODEL", model);
    }
    let head = git(root, &["rev-parse", "HEAD"])?;
    let branch = git(root, &["rev-parse", "--symbolic-full-name", "HEAD"])?;
    let ledger_path = root.join("harness/ledger.jsonl");
    let ledger_before = std::fs::read(&ledger_path).ok();
    begin_improvement(root, &args.run.binary, &note, &head)?;
    let result = cmd.status();
    if git(root, &["rev-parse", "HEAD"])? != head
        || git(root, &["rev-parse", "--symbolic-full-name", "HEAD"])? != branch
    {
        bail!("meta agent changed HEAD or branch; inspect the repository before continuing");
    }
    let checked = (|| -> Result<()> {
        // Restore unauthorized edits and preserve this loop's generated ledger.
        let note_relative = note
            .strip_prefix(root)?
            .to_str()
            .context("note path is not UTF-8")?;
        let mut allowed_files = lane.files().to_vec();
        allowed_files.push(note_relative);
        clean_outside(root, &allowed_files)?;
        if let Some(bytes) = ledger_before {
            std::fs::write(&ledger_path, bytes)?;
        }
        let status = result?;
        anyhow::ensure!(status.success(), "meta agent failed ({status})");
        let check = Command::new("cargo")
            .args(["check", "--workspace"])
            .current_dir(root)
            .status()?;
        anyhow::ensure!(check.success(), "improvement does not compile ({check})");
        Ok(())
    })();
    if let Err(error) = checked {
        if let Err(cleanup) = rollback_unscored(root, &args.run.binary, &note) {
            return Err(error.context(format!("{cleanup:#}")));
        }
        finish_improvement(root)?;
        return Err(error);
    }
    Ok(note)
}

fn proposal_baseline(root: &Path) -> Result<Vec<String>> {
    [
        vec!["rev-parse", "HEAD"],
        vec!["rev-parse", "--symbolic-full-name", "HEAD"],
        vec!["status", "--porcelain=v1", "--untracked-files=all"],
        vec!["diff", "--no-ext-diff", "--binary"],
        vec!["diff", "--cached", "--no-ext-diff", "--binary"],
    ]
    .iter()
    .map(|args| git(root, args))
    .collect()
}

fn improve_isolated_prompt(
    root: &Path,
    args: &IterateArgs,
    report: &Path,
    note: &Path,
    port: u16,
) -> Result<PathBuf> {
    use std::io::Write;
    let prompt_path = root.join("crates/rigcoder/src/prompt.md");
    anyhow::ensure!(
        prompt_path.canonicalize()? == prompt_path
            && std::fs::symlink_metadata(&prompt_path)?.is_file(),
        "prompt must be a regular repository file"
    );
    let prompt = std::fs::read_to_string(&prompt_path)?;
    let development = std::fs::read_to_string(report)?;
    let binary = host_binary(root)?.canonicalize()?;
    let baseline = proposal_baseline(root)?;
    let evidence = report.with_file_name("prompt-proposal");
    begin_improvement(root, &args.run.binary, note, &baseline[0])?;
    let proposal = crate::prompt_proposal::launch(&binary, &prompt, &development, port, &evidence);
    // External edits must survive. Keep the recovery marker and do not invoke
    // the legacy broad cleanup if the repository changed during proposal work.
    anyhow::ensure!(
        proposal_baseline(root)? == baseline && std::fs::read_to_string(&prompt_path)? == prompt,
        "repository changed during isolated proposal; retained candidate and recovery marker for inspection"
    );
    let proposal = match proposal {
        Ok(proposal) => proposal,
        Err(error) => {
            finish_improvement(root)?;
            return Err(error);
        }
    };
    let applied = (|| -> Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(note)?;
        output.write_all(
            proposal
                .note
                .as_deref()
                .unwrap_or("No rationale supplied.\n")
                .as_bytes(),
        )?;
        output.sync_all()?;
        std::fs::write(&prompt_path, proposal.prompt)?;
        Ok(())
    })();
    if let Err(error) = applied {
        rollback_unscored(root, &args.run.binary, note)?;
        finish_improvement(root)?;
        return Err(error);
    }
    // Only prompt text is applied here. The existing generation path builds
    // and evaluates it; proposal collection does not make a keep decision.
    Ok(note.to_owned())
}

fn paths(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    Ok(git(root, args)?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

fn changed_paths(root: &Path) -> Result<Vec<String>> {
    // Keep the index and worktree comparisons separate. A staged edit can
    // cancel an unstaged edit, leaving the worktree equal to HEAD while the
    // forbidden index contents would still enter the next commit.
    let mut changed = paths(
        root,
        &["diff", "--cached", "--name-only", "--no-renames", "-z"],
    )?;
    changed.extend(paths(root, &["diff", "--name-only", "--no-renames", "-z"])?);
    changed.extend(paths(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?);
    changed.sort();
    changed.dedup();
    Ok(changed)
}

fn prepare_evolve(root: &Path) -> Result<()> {
    let dirty: Vec<String> = changed_paths(root)?
        .into_iter()
        .filter(|p| p != "harness/ledger.jsonl")
        .collect();
    // Even generated ledger changes must not enter a candidate's commit.
    let staged = git(root, &["diff", "--cached", "--name-only"])?;
    if !dirty.is_empty() || !staged.is_empty() {
        bail!(
            "self-improvement requires a clean index and worktree (generated unstaged ledger excepted): {dirty:?}"
        );
    }
    if git(root, &["rev-parse", "--abbrev-ref", "HEAD"])? != "evolve" {
        let exists = !git(root, &["branch", "--list", "evolve"])?.is_empty();
        if exists {
            bail!(
                "evolve already exists; check it out explicitly to resume (it will not be reset)"
            );
        }
        git(root, &["checkout", "-b", "evolve"])?;
    }
    Ok(())
}

fn restore_paths(root: &Path, restore: &[String]) -> Result<()> {
    let tracked: BTreeSet<String> = paths(root, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])?
        .into_iter()
        .collect();
    for path in restore {
        // Reset the index first, including newly staged files, then restore
        // tracked paths or remove files created by this candidate.
        git(root, &["reset", "-q", "HEAD", "--", path])?;
        if tracked.contains(path) {
            git(
                root,
                &["restore", "--source=HEAD", "--worktree", "--", path],
            )?;
        } else {
            let file = root.join(path);
            match std::fs::symlink_metadata(&file) {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&file)?,
                Ok(_) => std::fs::remove_file(&file)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn clean_outside(root: &Path, files: &[&str]) -> Result<()> {
    let outside: Vec<String> = changed_paths(root)?
        .into_iter()
        .filter(|path| !covered(files, path))
        .collect();
    restore_paths(root, &outside)
}

fn revert_mutable(root: &Path) -> Result<()> {
    let restore: Vec<String> = changed_paths(root)?
        .into_iter()
        .filter(|path| covered(MUTABLE, path))
        .collect();
    restore_paths(root, &restore)
}

fn rollback_unscored(root: &Path, binary: &Path, note: &Path) -> Result<()> {
    // Attempt source and binary cleanup even if evidence storage fails.
    let archive = (|| -> Result<()> {
        let relative = note
            .strip_prefix(root)?
            .to_str()
            .context("note path is not UTF-8")?;
        let indexed = !git(root, &["ls-files", "--stage", "--", relative])?.is_empty();
        if note.is_file() || indexed {
            let dir = new_job_dir(root, None, "unscored-candidate")?;
            if note.is_file() {
                std::fs::copy(note, dir.join("improvement.md"))?;
            }
            if indexed {
                // Preserve exact bytes, including final newlines. Worktree and
                // index can contain different versions of the same note.
                let output = Command::new("git")
                    .args(["show", &format!(":{relative}")])
                    .current_dir(root)
                    .output()?;
                anyhow::ensure!(
                    output.status.success(),
                    "reading staged improvement note failed"
                );
                std::fs::write(dir.join("improvement-index.md"), output.stdout)?;
            }
        }
        // A note can exist only in the index after a staged deletion.
        restore_paths(root, &[relative.to_owned()])?;
        Ok(())
    })();
    let invalidate = remove_build_output(&root.join(binary));
    let restore = revert_mutable(root);
    let errors: Vec<String> = [archive, invalidate, restore]
        .into_iter()
        .filter_map(|result| result.err().map(|error| format!("{error:#}")))
        .collect();
    anyhow::ensure!(
        errors.is_empty(),
        "unscored candidate cleanup failed: {}",
        errors.join("; ")
    );
    Ok(())
}

pub fn iterate(root: &Path, args: IterateArgs) -> Result<()> {
    require_no_pending_improvement(root)?;
    if args.meta_gateway_port.is_some() {
        anyhow::ensure!(
            cfg!(target_os = "macos"),
            "isolated prompt improvement requires macOS"
        );
        anyhow::ensure!(
            !args.no_improve
                && args.lane == Some(Lane::Prompt)
                && args.meta_provider == "gemini"
                && args
                    .meta_model
                    .as_deref()
                    .is_none_or(|model| model == "gemini-3.8-flash"),
            "--meta-gateway-port requires self-editing, --lane prompt and Gemini gemini-3.8-flash"
        );
        anyhow::ensure!(
            std::env::var("RIGCODER_GATEWAY_TOKEN").is_ok_and(|token| !token.is_empty()),
            "RIGCODER_GATEWAY_TOKEN is required"
        );
    }
    let run_args = &args.run;
    if run_args.slice == "holdout" && !args.no_improve {
        bail!("the holdout slice is for evaluation only: pass --no-improve");
    }
    let tasks = tasks_for(root, run_args)?;
    validate_run(run_args, &tasks)?;
    let slice = slice_name(run_args);
    let holdout = if !args.no_improve || (args.holdout && slice == "dev") {
        slices::read(root, "holdout")?
    } else {
        Vec::new()
    };
    if !args.no_improve {
        anyhow::ensure!(
            !tasks.iter().any(|task| holdout.contains(task)),
            "self-improvement tasks overlap the holdout; use disjoint development tasks"
        );
    }
    if args.generations == 0 {
        bail!("generations must be greater than zero");
    }
    if !args.no_improve {
        if run_args.no_build {
            bail!(
                "--no-build requires --no-improve: self-edits must rebuild the benchmarked binary"
            );
        }
        prepare_evolve(root)?;
    }
    // Every invocation evaluates its own baseline. Historical scores may
    // use other models, slices, task versions, or commits.
    let mut best = (-1.0, -1.0);
    let mut previous_lane = None;
    let mut lane_this_generation = None;
    let mut meta_commit = None;
    let mut note_this_generation: Option<PathBuf> = None;

    for generation in 0..args.generations {
        let evaluated = (|| {
            if !run_args.no_build {
                build_linux(root, &run_args.binary)?;
            }
            evaluate(
                root,
                run_args,
                &format!("gen-{generation:03}"),
                &tasks,
                if run_args.slice == "holdout" {
                    "holdout"
                } else {
                    "development"
                },
            )
        })();
        let (records, job_dir) = match evaluated {
            Ok(result) => result,
            Err(error) => {
                if let Some(note) = &note_this_generation
                    && let Err(cleanup) = rollback_unscored(root, &run_args.binary, note)
                {
                    return Err(error.context(format!("{cleanup:#}")));
                }
                if note_this_generation.is_some() {
                    finish_improvement(root)?;
                }
                return Err(error);
            }
        };
        let summary = stats::summarize(&records);
        let decision = stats::keep_decision(summary.score, summary.ci_low, best.0, best.1);
        let mut e = entry(
            root,
            Some(generation),
            &slice,
            Some(decision),
            best,
            run_args,
            &job_dir,
            summary.clone(),
        )?;
        e.lane = lane_this_generation;
        e.meta_commit = meta_commit.clone();
        let mut rebuild_rejected = false;
        if !args.no_improve {
            match decision {
                Decision::Kept | Decision::Tie => {
                    best = (summary.score, summary.ci_low);
                    let note_relative = note_this_generation
                        .as_ref()
                        .and_then(|path| path.strip_prefix(root).ok())
                        .and_then(Path::to_str);
                    let changed: Vec<String> = changed_paths(root)?
                        .into_iter()
                        .filter(|path| {
                            covered(MUTABLE, path) || note_relative == Some(path.as_str())
                        })
                        .collect();
                    if !changed.is_empty() {
                        let mut add = vec!["add", "--all", "--"];
                        add.extend(changed.iter().map(String::as_str));
                        git(root, &add)?;
                        let message = format!(
                            "evolve: generation {generation} scored {:.3} [{:.3}, {:.3}] ({decision:?})",
                            summary.score, summary.ci_low, summary.ci_high
                        );
                        git(root, &["commit", "-q", "-m", &message])?;
                        e.commit = git(root, &["rev-parse", "--short", "HEAD"])?;
                        rebuild_host(root)?;
                    }
                }
                Decision::Reverted => {
                    println!(
                        "generation {generation}: {:.3} [{:.3}] below best {:.3} [{:.3}]; reverting",
                        summary.score, summary.ci_low, best.0, best.1
                    );
                    revert_mutable(root)?;
                    // Bookkeeping can fail too; invalidate rejected behavior
                    // before archiving evidence or appending the ledger.
                    remove_build_output(&root.join(&run_args.binary))?;
                    // Restore the executable as well as source: a final rejection
                    // may have no later generation or holdout to trigger a build.
                    rebuild_rejected = true;
                    // Preserve rejected notes as job artifacts without including
                    // them in a later generation's commit.
                    if let Some(note) = &note_this_generation
                        && note.is_file()
                    {
                        std::fs::copy(note, job_dir.join("improvement.md"))?;
                        let relative = note
                            .strip_prefix(root)?
                            .to_str()
                            .context("note path is not UTF-8")?
                            .to_owned();
                        restore_paths(root, &[relative])?;
                    }
                }
            }
        } else {
            e.decision = None;
            best = (summary.score, summary.ci_low);
        }
        ledger::append(root, &e)?;
        if note_this_generation.is_some() {
            finish_improvement(root)?;
        }
        // Persist the decision and rejected note before a fallible rebuild.
        if rebuild_rejected {
            build_linux(root, &run_args.binary)?;
        }
        if args.no_improve || generation + 1 == args.generations {
            continue;
        }
        let report_path = report::write(
            &job_dir, root, generation, &summary, &records, best.0, best.1,
        )?;
        let digest = crate::digest::job(&job_dir, root).ok();
        let lane = args
            .lane
            .unwrap_or_else(|| Lane::choose(digest.as_ref(), previous_lane));
        println!("generation {}: lane {}", generation + 1, lane.name());
        meta_commit = Some(git(root, &["rev-parse", "--short", "HEAD"])?);
        note_this_generation = Some(improve(root, &args, &report_path, lane, generation + 1)?);
        previous_lane = Some(lane);
        lane_this_generation = Some(lane);
    }

    if args.holdout && slice == "dev" {
        // A final rejected candidate left its binary on disk; rebuild HEAD.
        if !run_args.no_build {
            build_linux(root, &run_args.binary)?;
        }
        let (records, job_dir) = evaluate(root, run_args, "holdout", &holdout, "holdout")?;
        let summary = stats::summarize(&records);
        ledger::append(
            root,
            &entry(
                root, None, "holdout", None, best, run_args, &job_dir, summary,
            )?,
        )?;
    }
    Ok(())
}

/// Replay a recorded trial's effect log on this machine through the host
/// `rigcoder`: exit 0 means the current prompt and tools reproduce the
/// recorded requests exactly; exit 3 means a divergence, printed with the
/// request that differed.
pub fn replay(
    root: &Path,
    trial_dir: &Path,
    prompt_file: Option<&Path>,
    host_bin: Option<&Path>,
) -> Result<()> {
    let log = trial_dir.join("agent").join("effects.json");
    if !log.is_file() {
        bail!("no effect log at {}", log.display());
    }
    let host_bin = host_bin
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("target").join("release").join("rigcoder"));
    // The recorded preamble names the container's workdir; the replay must
    // say the same string, and never touches it.
    let workdir = trial_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split("__").next())
        .and_then(|task| Task::load(&root.join("harness").join("tasks"), task).ok())
        .map(|t| t.workdir)
        .unwrap_or_else(|| "/app".to_owned());
    let scratch = PathBuf::from(&workdir);
    let mut cmd = Command::new(&host_bin);
    cmd.arg("--cwd")
        .arg(&scratch)
        .arg("--replay")
        .arg(&log)
        .arg("--provider")
        .arg("gemini");
    if let Some(prompt) = prompt_file {
        cmd.arg("--prompt-file").arg(prompt);
    }
    println!(
        "$ {} --cwd {} --replay {}{}",
        host_bin.display(),
        scratch.display(),
        log.display(),
        prompt_file.map_or(String::new(), |p| format!(" --prompt-file {}", p.display()))
    );
    let status = cmd.status()?;
    match status.code() {
        Some(0) => {
            println!(
                "no divergence: the recorded trajectory reproduces under the current prompt and tools"
            );
            Ok(())
        }
        Some(3) => {
            println!("divergence: a live run is needed from the turn printed above");
            std::process::exit(3)
        }
        other => bail!("replay exited with {other:?}"),
    }
}

/// `branch-from`: resume a recorded trial from a checkpoint `times` times.
pub fn branch_from(
    root: &Path,
    trial_dir: &Path,
    turn: usize,
    times: usize,
    args: &RunArgs,
) -> Result<()> {
    anyhow::ensure!(
        args.gemini_budget.is_none(),
        "budgeted branching is not supported; use fresh trials"
    );
    anyhow::ensure!(times > 0, "branch-from requires at least one attempt");
    docker::available()?;
    if !args.no_build {
        build_linux(root, &args.binary)?;
    }
    let task_name = trial_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split("__").next())
        .ok_or_else(|| anyhow::anyhow!("trial dir name is not <task>__<attempt>"))?;
    let task = Task::load(&root.join(&args.tasks_dir), task_name)?;
    let (provider, model) = args
        .model
        .split_once('/')
        .unwrap_or(("gemini", &args.model));
    let key_name = provider_key(provider);
    let key_value = key_name.and_then(|k| std::env::var(k).ok());
    anyhow::ensure!(
        key_name.is_none() || key_value.is_some(),
        "provider API key is not set"
    );
    let binary = root.join(&args.binary);
    anyhow::ensure!(
        binary.is_file(),
        "missing Linux rigcoder binary: {}",
        binary.display()
    );
    let out_dir = new_job_dir(
        root,
        args.job_name.as_deref(),
        &format!("branch-{task_name}"),
    )?;
    let passed = trial::branch_from(
        &task,
        trial_dir,
        turn,
        times,
        &binary,
        provider,
        model,
        key_name.zip(key_value.as_deref()),
        args.max_turns,
        &out_dir,
    )?;
    println!(
        "branch from turn {turn}: {passed}/{times} passed; runs under {}",
        out_dir.display()
    );
    Ok(())
}

/// The host `rigcoder` the improve step runs as: `RIGCODER_HOST_BIN`, or
/// `target/release/rigcoder`, built when missing.
fn host_binary(root: &Path) -> Result<PathBuf> {
    if let Ok(path) = std::env::var("RIGCODER_HOST_BIN") {
        return Ok(PathBuf::from(path));
    }
    let host_bin = root.join("target").join("release").join("rigcoder");
    if !host_bin.is_file() {
        rebuild_host(root)?;
    }
    Ok(host_bin)
}

/// Rebuild the host binary so the next improve step runs as the improved
/// agent: improvements compound into the improver.
fn rebuild_host(root: &Path) -> Result<()> {
    if std::env::var("RIGCODER_HOST_BIN").is_ok() {
        return Ok(());
    }
    println!("$ cargo build --release -p rigcoder-cli");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "rigcoder-cli"])
        .current_dir(root)
        .status()?;
    if !status.success() {
        bail!("building the host rigcoder failed");
    }
    Ok(())
}

#[cfg(test)]
mod lane_tests {
    use super::*;
    use crate::digest::{Aggregate, Digest};

    fn digest(failed: Aggregate, passed: Aggregate) -> Digest {
        Digest {
            evaluation: None,
            failed,
            passed,
            trials: Vec::new(),
        }
    }

    #[test]
    fn unknown_usage_cannot_supply_a_shaping_signal() {
        let failed = Aggregate {
            trials: 2,
            input_tokens: Some(200.0),
            ..Default::default()
        };
        let passed = Aggregate {
            trials: 2,
            input_tokens: Some(100.0),
            ..Default::default()
        };
        let mut evidence = digest(failed, passed);
        assert_eq!(Lane::choose(Some(&evidence), None), Lane::Shaping);
        evidence.failed.input_tokens = None;
        assert_eq!(Lane::choose(Some(&evidence), None), Lane::Prompt);
        evidence.failed.input_tokens = Some(200.0);
        evidence.passed.input_tokens = None;
        assert_eq!(Lane::choose(Some(&evidence), None), Lane::Prompt);
    }

    #[test]
    fn timeouts_point_at_tools_and_deliberation_at_prompt() {
        let f = Aggregate {
            trials: 2,
            timeouts: 1.0,
            ..Default::default()
        };
        let p = Aggregate {
            trials: 4,
            ..Default::default()
        };
        assert_eq!(Lane::choose(Some(&digest(f, p)), None), Lane::Tools);
        let f = Aggregate {
            trials: 2,
            ended_deliberating: 1.0,
            ..Default::default()
        };
        let p = Aggregate {
            trials: 4,
            ended_deliberating: 0.2,
            ..Default::default()
        };
        assert_eq!(Lane::choose(Some(&digest(f, p)), None), Lane::Prompt);
    }

    #[test]
    fn no_signal_round_robins_and_no_failures_too() {
        assert_eq!(Lane::choose(None, None), Lane::Prompt);
        assert_eq!(Lane::choose(None, Some(Lane::Prompt)), Lane::Tools);
        assert_eq!(Lane::choose(None, Some(Lane::Systems)), Lane::Prompt);
        let d = digest(
            Aggregate::default(),
            Aggregate {
                trials: 3,
                ..Default::default()
            },
        );
        assert_eq!(Lane::choose(Some(&d), Some(Lane::Tools)), Lane::Settings);
    }

    #[test]
    fn coverage_handles_files_and_directories() {
        assert!(covered(Lane::Tools.files(), "crates/rigcoder/src/tools.rs"));
        assert!(covered(
            Lane::Tools.files(),
            "crates/rigcoder/src/tools/bash.rs"
        ));
        assert!(!covered(Lane::Tools.files(), "crates/rigcoder/src/lib.rs"));
        assert!(!covered(Lane::Prompt.files(), "harness/ledger.jsonl"));
    }
}
