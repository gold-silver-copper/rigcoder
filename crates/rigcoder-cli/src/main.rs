//! Headless rigcoder: one task, run to completion, the transcript on
//! stdout (and as JSON lines to a file), exit 0 when the run settles.

use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

use bevy_app::{App, AppExit, PostStartup, ScheduleRunnerPlugin, Update};
use bevy_ecs::prelude::*;
use clap::Parser;
use rigcoder::{Event, ModelChoice, RigcoderPlugin, Transcript};

#[derive(Parser, Debug)]
#[command(name = "rigcoder", about = "A coding agent over rig-ecs, headless.")]
struct Args {
    /// The task. Reads stdin when absent.
    task: Option<String>,
    /// Read the task from a file instead.
    #[arg(long)]
    task_file: Option<PathBuf>,
    /// Workspace directory (default: the current directory).
    #[arg(long, short = 'C')]
    cwd: Option<PathBuf>,
    /// Provider: anthropic or openai.
    #[arg(long, default_value = "anthropic", env = "RIGCODER_PROVIDER")]
    provider: String,
    /// Model name (provider default when absent).
    #[arg(long, env = "RIGCODER_MODEL")]
    model: Option<String>,
    /// Model calls per run.
    #[arg(long, default_value_t = 200)]
    max_turns: usize,
    /// Write every transcript event as a JSON line here.
    #[arg(long)]
    transcript: Option<PathBuf>,
    /// Give up after this many seconds.
    #[arg(long)]
    timeout_secs: Option<u64>,
    /// Print tool output in full instead of the first lines.
    #[arg(long)]
    verbose: bool,
}

#[derive(Resource)]
struct Cli {
    task: String,
    printed: usize,
    transcript: Option<std::fs::File>,
    started: Instant,
    deadline: Option<Duration>,
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let task = match (&args.task, &args.task_file) {
        (Some(task), _) => task.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)?,
        (None, None) => {
            let mut task = String::new();
            std::io::stdin().read_line(&mut task)?;
            let mut rest = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut rest)?;
            task.push_str(&rest);
            task
        }
    };
    let task = task.trim().to_owned();
    anyhow::ensure!(!task.is_empty(), "an empty task");
    let workspace = match args.cwd {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    }
    .canonicalize()?;
    let model = ModelChoice::parse(&args.provider, args.model)?;
    let transcript = args.transcript.map(std::fs::File::create).transpose()?;
    eprintln!("rigcoder: {model} in {}", workspace.display());

    let exit = App::new()
        .add_plugins((
            ScheduleRunnerPlugin::run_loop(Duration::from_millis(10)),
            RigcoderPlugin {
                workspace,
                model,
                max_turns: args.max_turns,
            },
        ))
        .insert_resource(Cli {
            task,
            printed: 0,
            transcript,
            started: Instant::now(),
            deadline: args.timeout_secs.map(Duration::from_secs),
            verbose: args.verbose,
        })
        .add_systems(PostStartup, start)
        .add_systems(Update, (report, watchdog))
        .run();
    match exit {
        AppExit::Success => Ok(()),
        AppExit::Error(code) => std::process::exit(code.get() as i32),
    }
}

fn start(world: &mut World) {
    let task = world.resource::<Cli>().task.clone();
    if rigcoder::submit(world, &task).is_none() {
        eprintln!("rigcoder: could not start the run (no model registered?)");
        world.write_message(AppExit::error());
    }
}

/// Print what happened since the last tick; exit when the run ends.
fn report(transcript: Res<Transcript>, mut cli: ResMut<Cli>, mut exit: MessageWriter<AppExit>) {
    let mut stdout = std::io::stdout().lock();
    while cli.printed < transcript.events.len() {
        let event = &transcript.events[cli.printed];
        // A streaming answer grows in place while it is the last event:
        // print it once something follows it (or the run ends).
        if matches!(event, Event::Assistant { .. }) && cli.printed + 1 == transcript.events.len() {
            break;
        }
        cli.printed += 1;
        if let Some(file) = cli.transcript.as_mut() {
            if serde_json::to_writer(&mut *file, event).is_ok() {
                let _ = file.write_all(b"\n");
            }
        }
        match event {
            Event::User { text } => {
                let _ = writeln!(stdout, "> {text}\n");
            }
            Event::Assistant { text } => {
                let _ = writeln!(stdout, "{text}\n");
            }
            Event::ToolCall { name, args } => {
                let _ = writeln!(stdout, "[tool] {name} {}", short(args, 300));
            }
            Event::ToolResult { name, output, ok } => {
                let shown = if cli.verbose { output.clone() } else { short(output, 600) };
                let _ = writeln!(
                    stdout,
                    "[{}] {name}\n{shown}\n",
                    if *ok { "result" } else { "error" }
                );
            }
            Event::Settled { .. } => {
                let _ = writeln!(stdout, "[settled after {:?}]", cli.started.elapsed());
                exit.write(AppExit::Success);
            }
            Event::Failed(reason) => {
                let _ = writeln!(stdout, "[failed] {reason}");
                exit.write(AppExit::error());
            }
        }
    }
    let _ = stdout.flush();
}

fn watchdog(world: &mut World) {
    let Some(deadline) = world.resource::<Cli>().deadline else {
        return;
    };
    if world.resource::<Cli>().started.elapsed() > deadline {
        rigcoder::cancel(world, "rigcoder --timeout-secs elapsed");
    }
}

fn short(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.len() < text.len() {
        out.push_str(" …");
    }
    out
}
