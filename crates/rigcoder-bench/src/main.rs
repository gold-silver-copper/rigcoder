//! `rigcoder-bench`: the benchmark runner and the self-improvement loop.
//!
//! A Terminal-Bench task is a directory: `task.toml` (timeouts, resources),
//! `instruction.md`, `environment/Dockerfile`, `tests/test.sh` which writes
//! `/logs/verifier/reward.txt`. This binary builds the image, runs the
//! container, copies the Linux `rigcoder` in, runs it on the instruction,
//! runs the verifier, and reads the reward. No framework in between: six
//! `docker` calls per trial.

mod digest;
mod docker;
mod evolve;
mod ledger;
mod report;
mod slices;
mod stats;
mod task;
mod trial;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rigcoder-bench",
    about = "Terminal-Bench runner and evolve loop for rigcoder."
)]
struct Cli {
    /// Repository root (default: the current directory).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Evaluate the current binary on a slice: build images, run trials, record the job.
    Run(evolve::RunArgs),
    /// The loop: evaluate, keep or revert, let rigcoder edit itself, repeat; then score holdout.
    Iterate(evolve::IterateArgs),
    /// Print the ledger as a table.
    Ledger,
    /// Summarize one job directory (scores, interval, per-task attempts).
    Summarize { job_dir: PathBuf },
    /// The failure digest of one job directory, as markdown; writes digest.json beside it.
    Digest { job_dir: PathBuf },
    /// Replay a recorded trial on this machine, no model and no side effects: reports the first divergence, if any.
    Replay {
        trial_dir: PathBuf,
        /// Use this system prompt instead of the compiled-in one (a proposed edit).
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        /// The host rigcoder binary (default target/release/rigcoder).
        #[arg(long)]
        host_bin: Option<PathBuf>,
    },
    /// Resume a recorded trial from a checkpoint in fresh containers and count verifier passes.
    BranchFrom {
        trial_dir: PathBuf,
        turn: usize,
        #[arg(long, default_value_t = 3)]
        times: usize,
        #[command(flatten)]
        run: evolve::RunArgs,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = match cli.root {
        Some(root) => root,
        None => std::env::current_dir()?,
    }
    .canonicalize()?;
    match cli.command {
        Command::Run(args) => evolve::run(&root, args),
        Command::Iterate(args) => evolve::iterate(&root, args),
        Command::Ledger => ledger::print(&root),
        Command::Digest { job_dir } => {
            let (digest, path) = digest::write(&job_dir)?;
            print!("{}", digest::render(&digest));
            eprintln!("wrote {}", path.display());
            Ok(())
        }
        Command::Replay {
            trial_dir,
            prompt_file,
            host_bin,
        } => evolve::replay(
            &root,
            &trial_dir,
            prompt_file.as_deref(),
            host_bin.as_deref(),
        ),
        Command::BranchFrom {
            trial_dir,
            turn,
            times,
            run,
        } => evolve::branch_from(&root, &trial_dir, turn, times, &run),
        Command::Summarize { job_dir } => {
            let trials = trial::read_job(&job_dir)?;
            let summary = stats::summarize(&trials);
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
    }
}
