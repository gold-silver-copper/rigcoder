//! `run`: evaluate a slice. `iterate`: the self-improvement loop over it.

use std::{
    collections::VecDeque,
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

/// What the improvement step is allowed to change. Everything else is the
/// harness's, not the agent's.
pub const MUTABLE: &[&str] = &[
    "crates/rigcoder/src/prompt.md",
    "crates/rigcoder/src/tools.rs",
    "crates/rigcoder/src/lib.rs",
    "crates/rigcoder/src/session.rs",
    "crates/rigcoder-cli/src/main.rs",
];

/// What the meta agent must never read: the held-out tasks, its own scores,
/// and the harness that scores it.
pub const META_FORBIDDEN: &[&str] = &["harness/slices/holdout.txt", "harness/ledger.jsonl", "crates/rigcoder-bench/"];

const META_TASK: &str = "You are improving rigcoder, the coding agent in this repository, so it scores higher on Terminal-Bench.

Read {report} first: it has the benchmark results of the current version, and for every failed task the instruction, the tail of the agent's transcript, and the verifier's output.

Then change the agent. You may only edit these files: {mutable}. Typical levers, in order of leverage: the system prompt (prompt.md: process, verification habits, when to stop), tool descriptions and behaviours (tools.rs: output limits, timeouts, error messages the model can act on), the agent's settings (lib.rs: max tokens, tool concurrency), and how tool results are shaped (session.rs).

Rules:
- Make one coherent improvement aimed at the failure patterns you see, not many unrelated tweaks.
- Do not touch the harness/ directory, the rigcoder-bench crate, Cargo.toml files, or the model choice. Do not read {forbidden}.
- Run `cargo check --workspace` with bash and make it pass before you finish.
- Finish with a short note: what you changed and which failures it targets. Write that note to {note}.
";

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
    /// Model calls per run inside the container.
    #[arg(long, default_value_t = 200)]
    pub max_turns: usize,
    /// Task directories (a checkout of laude-institute/terminal-bench-2).
    #[arg(long, default_value = "harness/tasks")]
    pub tasks_dir: PathBuf,
    /// The Linux rigcoder binary to upload.
    #[arg(long, default_value = "harness/bin/rigcoder-linux-aarch64")]
    pub binary: PathBuf,
    /// Skip harness/build-linux.sh.
    #[arg(long)]
    pub no_build: bool,
    /// Job name (default: <label>-<unix time>).
    #[arg(long)]
    pub job_name: Option<String>,
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
    /// Evaluate only; never self-edit.
    #[arg(long)]
    pub no_improve: bool,
    /// Score the best kept generation on the holdout slice at the end.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub holdout: bool,
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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).current_dir(root).output().context("git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn build_linux(root: &Path) -> Result<()> {
    println!("$ bash harness/build-linux.sh");
    let status = Command::new("bash").arg("harness/build-linux.sh").current_dir(root).status()?;
    if !status.success() {
        bail!("harness/build-linux.sh failed");
    }
    Ok(())
}

/// Evaluate `tasks` with `attempts` each, `concurrency` at a time.
pub fn evaluate(root: &Path, args: &RunArgs, label: &str, tasks: &[String]) -> Result<(Vec<TrialRecord>, PathBuf)> {
    docker::available()?;
    let job = args.job_name.clone().unwrap_or_else(|| format!("{label}-{}", now()));
    let job_dir = root.join("harness").join("runs").join(&job);
    std::fs::create_dir_all(&job_dir)?;
    let (provider, model) = args.model.split_once('/').unwrap_or(("gemini", &args.model));
    let key_name = provider_key(provider);
    let key_value = key_name.and_then(|k| std::env::var(k).ok());
    if key_name.is_some() && key_value.is_none() {
        bail!("{} is not set", key_name.unwrap_or("the provider key"));
    }
    let binary = root.join(&args.binary);
    if !binary.is_file() {
        bail!("{} is missing; run harness/build-linux.sh", binary.display());
    }
    let tasks_dir = root.join(&args.tasks_dir);
    let loaded: Vec<Task> = tasks.iter().map(|name| Task::load(&tasks_dir, name)).collect::<Result<_>>()?;
    println!("job {job}: {} task(s) × {} attempt(s), {} at a time, {}", loaded.len(), args.attempts, args.concurrency, args.model);

    // Images first, in parallel: docker serializes what it must.
    let build_errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let queue: Mutex<VecDeque<&Task>> = Mutex::new(loaded.iter().collect());
    std::thread::scope(|scope| {
        for _ in 0..args.concurrency.max(1) {
            scope.spawn(|| {
                loop {
                    let Some(task) = queue.lock().unwrap().pop_front() else { break };
                    println!("building {} ({}, {}s agent timeout)", task.name, task.difficulty, task.agent_timeout_secs);
                    if let Err(error) = trial::build_image(task) {
                        build_errors.lock().unwrap().push(format!("{error:#}"));
                    }
                }
            });
        }
    });
    let build_errors = build_errors.into_inner().unwrap();
    for error in &build_errors {
        eprintln!("{error}");
    }

    let mut specs: Vec<(usize, &Task)> = Vec::new();
    for attempt in 1..=args.attempts {
        for task in &loaded {
            if !build_errors.iter().any(|e| e.contains(&format!("for {} ", task.name))) {
                specs.push((attempt, task));
            }
        }
    }
    let results: Mutex<Vec<TrialRecord>> = Mutex::new(Vec::new());
    let total = specs.len();
    let queue: Mutex<VecDeque<(usize, &Task)>> = Mutex::new(specs.into_iter().collect());
    std::thread::scope(|scope| {
        for _ in 0..args.concurrency.max(1) {
            scope.spawn(|| {
                loop {
                    let Some((attempt, task)) = queue.lock().unwrap().pop_front() else { break };
                    let spec = TrialSpec {
                        task,
                        attempt,
                        job_dir: &job_dir,
                        binary: &binary,
                        provider,
                        model,
                        api_key: key_name.zip(key_value.as_deref()),
                        max_turns: args.max_turns,
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
                        record.error.as_ref().map_or(String::new(), |e| format!(" ERROR {e}"))
                    );
                }
            });
        }
    });
    let mut records = results.into_inner().unwrap();
    records.sort_by(|a, b| (&a.task, a.attempt).cmp(&(&b.task, b.attempt)));
    let summary = stats::summarize(&records);
    std::fs::write(job_dir.join("summary.json"), serde_json::to_string_pretty(&summary)?)?;
    println!(
        "job {job}: score {:.3} [{:.3}, {:.3}] pass@1 {:.3} pass@k {:.3} over {} trials",
        summary.score, summary.ci_low, summary.ci_high, summary.pass1, summary.passk, summary.trials
    );
    Ok((records, job_dir))
}

fn slice_name(args: &RunArgs) -> String {
    if args.include.is_empty() { args.slice.clone() } else { "custom".to_owned() }
}

fn tasks_for(root: &Path, args: &RunArgs) -> Result<Vec<String>> {
    if args.include.is_empty() { slices::read(root, &args.slice) } else { Ok(args.include.clone()) }
}

pub fn run(root: &Path, args: RunArgs) -> Result<()> {
    if !args.no_build {
        build_linux(root)?;
    }
    let tasks = tasks_for(root, &args)?;
    let (records, job_dir) = evaluate(root, &args, &format!("run-{}", slice_name(&args)), &tasks)?;
    let summary = stats::summarize(&records);
    ledger::append(root, &entry(root, None, &slice_name(&args), None, (-1.0, -1.0), &args, &job_dir, summary)?)?;
    Ok(())
}

fn entry(root: &Path, generation: Option<usize>, slice: &str, decision: Option<Decision>, best: (f64, f64), args: &RunArgs, job_dir: &Path, summary: Summary) -> Result<ledger::Entry> {
    Ok(ledger::Entry {
        generation,
        slice: slice.to_owned(),
        commit: git(root, &["rev-parse", "--short", "HEAD"])?,
        decision,
        best_score: best.0,
        best_ci_low: best.1,
        model: args.model.clone(),
        attempts: args.attempts,
        job_dir: job_dir.display().to_string(),
        time: now(),
        summary,
    })
}

fn improve(root: &Path, args: &IterateArgs, report_path: &Path) -> Result<()> {
    let note = report_path.with_file_name("improvement.md");
    let task = META_TASK
        .replace("{report}", &report_path.display().to_string())
        .replace("{mutable}", &MUTABLE.join(", "))
        .replace("{forbidden}", &META_FORBIDDEN.join(", "))
        .replace("{note}", &note.display().to_string());
    let host_bin = std::env::var("RIGCODER_HOST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("target").join("release").join("rigcoder"));
    if !host_bin.is_file() {
        println!("$ cargo build --release -p rigcoder-cli");
        let status = Command::new("cargo").args(["build", "--release", "-p", "rigcoder-cli"]).current_dir(root).status()?;
        if !status.success() {
            bail!("building the host rigcoder failed");
        }
    }
    println!("$ {} --cwd {} (meta task, {} chars)", host_bin.display(), root.display(), task.len());
    let mut cmd = Command::new(&host_bin);
    cmd.arg("--cwd")
        .arg(root)
        .args(["--max-turns", "80", "--timeout-secs", "1800", "--transcript"])
        .arg(report_path.with_file_name("improve-transcript.jsonl"))
        .arg(&task)
        .env("RIGCODER_PROVIDER", &args.meta_provider)
        .current_dir(root);
    if let Some(model) = &args.meta_model {
        cmd.env("RIGCODER_MODEL", model);
    }
    let _ = cmd.status()?;
    // Whatever the meta run did outside the mutable set is undone.
    let changed = git(root, &["diff", "--name-only"])?;
    let outside: Vec<&str> = changed.lines().filter(|f| !MUTABLE.contains(f)).collect();
    if !outside.is_empty() {
        println!("meta agent touched non-mutable files, reverting: {outside:?}");
        let mut argv = vec!["checkout", "--"];
        argv.extend(outside);
        git(root, &argv)?;
    }
    // Refuse a change that does not compile: revert to the last kept state.
    let check = Command::new("cargo").args(["check", "--workspace"]).current_dir(root).status()?;
    if !check.success() {
        println!("improvement does not compile; reverting");
        let mut argv = vec!["checkout", "--"];
        argv.extend(MUTABLE);
        git(root, &argv)?;
    }
    Ok(())
}

pub fn iterate(root: &Path, args: IterateArgs) -> Result<()> {
    let run_args = &args.run;
    if run_args.slice == "holdout" && !args.no_improve {
        bail!("the holdout slice is for evaluation only: pass --no-improve");
    }
    let tasks = tasks_for(root, run_args)?;
    // Self-edits are committed on `evolve`; an evaluation-only run stays on
    // whatever branch it was started from.
    if !args.no_improve && git(root, &["rev-parse", "--abbrev-ref", "HEAD"])? != "evolve" {
        git(root, &["checkout", "-B", "evolve"])?;
    }
    let mut best = ledger::best_kept(root)?
        .map(|e| (e.summary.score, e.summary.ci_low))
        .unwrap_or((-1.0, -1.0));
    let slice = slice_name(run_args);

    for generation in 0..args.generations {
        if !run_args.no_build {
            build_linux(root)?;
        }
        let (records, job_dir) = evaluate(root, run_args, &format!("generation-{generation:03}"), &tasks)?;
        let summary = stats::summarize(&records);
        let decision = stats::keep_decision(summary.score, summary.ci_low, best.0, best.1);
        let mut e = entry(root, Some(generation), &slice, Some(decision), best, run_args, &job_dir, summary.clone())?;
        match decision {
            Decision::Kept | Decision::Tie => {
                best = (summary.score, summary.ci_low);
                let mut status = vec!["status", "--porcelain", "--"];
                status.extend(MUTABLE);
                if !git(root, &status)?.is_empty() {
                    let mut add = vec!["add", "--"];
                    add.extend(MUTABLE);
                    git(root, &add)?;
                    let message = format!("evolve: generation {generation} scored {:.3} [{:.3}, {:.3}] ({decision:?})", summary.score, summary.ci_low, summary.ci_high);
                    git(root, &["commit", "-q", "-m", &message])?;
                    e.commit = git(root, &["rev-parse", "--short", "HEAD"])?;
                }
            }
            Decision::Reverted => {
                println!("generation {generation}: {:.3} [{:.3}] below best {:.3} [{:.3}]; reverting", summary.score, summary.ci_low, best.0, best.1);
                let mut argv = vec!["checkout", "--"];
                argv.extend(MUTABLE);
                git(root, &argv)?;
            }
        }
        ledger::append(root, &e)?;
        if args.no_improve || generation + 1 == args.generations {
            continue;
        }
        let report_path = report::write(&job_dir, generation, &summary, &records, best.0, best.1)?;
        improve(root, &args, &report_path)?;
    }

    if args.holdout && slice == "dev" {
        let holdout = slices::read(root, "holdout")?;
        let (records, job_dir) = evaluate(root, run_args, "holdout", &holdout)?;
        let summary = stats::summarize(&records);
        ledger::append(root, &entry(root, None, "holdout", None, best, run_args, &job_dir, summary)?)?;
    }
    Ok(())
}
