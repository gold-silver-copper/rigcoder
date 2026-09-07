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
    /// Provider: anthropic, openai or gemini.
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
    /// A file the task must produce; a text-only answer while it is missing is retried (repeatable).
    #[arg(long = "deliverable")]
    deliverables: Vec<PathBuf>,
    /// An extra regex; a bash command matching it is denied (repeatable).
    #[arg(long = "deny")]
    deny: Vec<String>,
    /// A regex; a bash command matching it is held and, headless, approved after logging (repeatable).
    #[arg(long = "hold")]
    hold: Vec<String>,
    /// Write the effect log (every model exchange and tool call, replayable without keys) here at exit.
    #[arg(long)]
    effect_log: Option<PathBuf>,
    /// Replay a recorded effect log instead of calling a provider: the model and the
    /// tools answer from the record; the first request that differs is reported as a divergence (exit 3).
    #[arg(long, conflicts_with = "resume")]
    replay: Option<PathBuf>,
    /// Use this file as the system prompt instead of the compiled-in one.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// Save the run graph (and, with --checkpoint-tar, the workspace) into this directory after every turn.
    #[arg(long)]
    checkpoint: Option<PathBuf>,
    /// Store archives outside the workspace to avoid recursive snapshots.
    #[arg(long, requires = "checkpoint")]
    checkpoint_tar: bool,
    /// Resume a saved scene in a fresh world (the workspace must already be as it was).
    #[arg(long)]
    resume: Option<PathBuf>,
    /// A path the file tools may write. Configuring scope disables bash (repeatable).
    #[arg(long = "allow")]
    allow: Vec<PathBuf>,
    /// A denied path; a more specific allow rule may name an exception (repeatable).
    #[arg(long = "deny-path")]
    deny_paths: Vec<PathBuf>,
}

#[derive(Resource)]
struct Resume(Option<rig_ecs::agent::scene::WorldScene>);

#[derive(Resource)]
struct EffectLogOut {
    path: Option<PathBuf>,
    written: bool,
}

/// Once the run has ended, write the effect log; the world is not readable
/// after `App::run` returns, so this runs inside the app.
fn write_effect_log(world: &mut World) {
    let over = {
        let c = world.resource::<rigcoder::Conversation>();
        c.runs > 0 && !c.is_busy()
    };
    let due = {
        let out = world.resource::<EffectLogOut>();
        over && !out.written && out.path.is_some()
    };
    if !due {
        return;
    }
    let path = world
        .resource::<EffectLogOut>()
        .path
        .clone()
        .expect("checked");
    let log = rigcoder::effect_log(world);
    let result = serde_json::to_vec(&log)
        .map_err(|error| error.to_string())
        .and_then(|json| write_log_atomically(&path, &json).map_err(|error| error.to_string()));
    if let Err(error) = result {
        eprintln!(
            "rigcoder: could not write the effect log to {}: {error}",
            path.display()
        );
        world.write_message(AppExit::error());
    }
    world.resource_mut::<EffectLogOut>().written = true;
}

fn write_log_atomically(path: &std::path::Path, json: &[u8]) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.partial", std::process::id()));
    let temporary = path.with_file_name(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(json)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// The first user message of the first recorded completion.
fn recorded_prompt(log: &rigcoder::EffectLog) -> Option<String> {
    log.iter().find_map(|record| match &record.kind {
        rig::effect::EffectKind::Completion { request, .. } => {
            request.chat_history.iter().find_map(|m| match m {
                rig::message::Message::User { content } => content.iter().find_map(|c| match c {
                    rig::message::UserContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                }),
                _ => None,
            })
        }
        _ => None,
    })
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
    let replay_log: Option<rigcoder::EffectLog> = match &args.replay {
        Some(path) => Some(serde_json::from_str(&std::fs::read_to_string(path)?)?),
        None => None,
    };
    let resume_scene: Option<rig_ecs::agent::scene::WorldScene> = match &args.resume {
        Some(path) => Some(serde_json::from_str(&std::fs::read_to_string(path)?)?),
        None => None,
    };
    let task = match (&args.task, &args.task_file, &replay_log, &resume_scene) {
        (_, _, Some(log), _) => recorded_prompt(log)
            .ok_or_else(|| anyhow::anyhow!("the effect log records no user prompt"))?,
        (_, _, _, Some(_)) => String::new(),
        (Some(task), ..) => task.clone(),
        (None, Some(path), ..) => std::fs::read_to_string(path)?,
        (None, None, ..) => {
            let mut task = String::new();
            std::io::stdin().read_line(&mut task)?;
            let mut rest = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut rest)?;
            task.push_str(&rest);
            task
        }
    };
    let task = task.trim().to_owned();
    anyhow::ensure!(!task.is_empty() || resume_scene.is_some(), "an empty task");
    let workspace = match args.cwd {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    // A replay never touches the directory: the path only has to be the
    // string the recorded preamble carried (a container's /app, say).
    let workspace = if args.replay.is_some() {
        workspace
    } else {
        workspace.canonicalize()?
    };
    let model = ModelChoice::parse(&args.provider, args.model)?;
    let transcript = args.transcript.map(std::fs::File::create).transpose()?;
    let deliverables = args
        .deliverables
        .iter()
        .map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                workspace.join(p)
            }
        })
        .collect();
    let mut steer = rigcoder::steer::Steer {
        deliverables,
        ..Default::default()
    };
    steer.deny.extend(
        args.deny
            .iter()
            .map(|p| (p.clone(), "denied by a --deny rule".to_owned())),
    );
    steer.hold.extend(args.hold.iter().cloned());
    steer.auto_approve = true;
    steer.validate()?;
    let absolute = |p: &PathBuf| {
        if p.is_absolute() {
            p.clone()
        } else {
            workspace.join(p)
        }
    };
    let scope = rigcoder::steer::Scope {
        root: workspace.clone(),
        allow: args.allow.iter().map(absolute).collect(),
        deny: args.deny_paths.iter().map(absolute).collect(),
    };
    eprintln!("rigcoder: {model} in {}", workspace.display());

    let effect_log_path = args.effect_log.clone();
    let prompt_override = args
        .prompt_file
        .as_ref()
        .map(std::fs::read_to_string)
        .transpose()?;
    let plugin = RigcoderPlugin {
        workspace: workspace.clone(),
        model,
        max_turns: args.max_turns,
        mode: match replay_log {
            Some(log) => rigcoder::Mode::Replay(log.into()),
            None => rigcoder::Mode::Live,
        },
        prompt_override,
    };
    let checkpoint = rigcoder::checkpoint::Checkpoint {
        dir: args.checkpoint.clone(),
        tar: args.checkpoint_tar,
        turns_saved: 0,
    };
    let mut app = App::new();
    app.add_plugins((
        ScheduleRunnerPlugin::run_loop(Duration::from_millis(10)),
        plugin,
    ))
    .insert_resource(checkpoint)
    .insert_resource(Resume(resume_scene))
    .insert_resource(steer)
    .insert_resource(scope)
    .insert_resource(Cli {
        task,
        printed: 0,
        transcript,
        started: Instant::now(),
        deadline: args.timeout_secs.map(Duration::from_secs),
        verbose: args.verbose,
    })
    .add_systems(PostStartup, start)
    .add_systems(Update, (report, watchdog));
    app.insert_resource(EffectLogOut {
        path: effect_log_path,
        written: false,
    })
    .add_systems(bevy_app::Last, write_effect_log);
    let exit = app.run();
    match exit {
        AppExit::Success => Ok(()),
        AppExit::Error(code) => std::process::exit(code.get() as i32),
    }
}

fn start(world: &mut World) {
    if let Some(scene) = world.resource_mut::<Resume>().0.take() {
        match rigcoder::checkpoint::resume(world, &scene) {
            Ok(_) => return,
            Err(report) => {
                eprintln!("rigcoder: could not resume: {report}");
                world.write_message(AppExit::error());
                return;
            }
        }
    }
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
        if let Some(file) = cli.transcript.as_mut()
            && serde_json::to_writer(&mut *file, event).is_ok()
        {
            let _ = file.write_all(b"\n");
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
                let shown = if cli.verbose {
                    output.clone()
                } else {
                    short(output, 600)
                };
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
                exit.write(
                    if reason.contains("replay")
                        || reason.contains("diverg")
                        || reason.contains("recorded")
                    {
                        AppExit::Error(std::num::NonZero::new(3).expect("nonzero"))
                    } else {
                        AppExit::error()
                    },
                );
            }
            Event::Denied { name, reason } => {
                let _ = writeln!(stdout, "[denied] {name}: {reason}");
            }
            Event::Retrying {
                reason,
                attempt,
                wait_secs,
            } => {
                let _ = writeln!(
                    stdout,
                    "[retrying in {wait_secs}s, attempt {attempt}] {}",
                    short(reason, 300)
                );
            }
            Event::Held { name, args } => {
                let _ = writeln!(stdout, "[held] {name} {}", short(args, 300));
            }
            Event::Usage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                ..
            } => {
                let _ = writeln!(
                    stdout,
                    "[usage] in={input_tokens} out={output_tokens} cached={cached_input_tokens}"
                );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rigcoder-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn requested_effect_log_failure_exits_with_an_error() {
        let mut app = App::new();
        app.world_mut()
            .insert_resource(rigcoder::Conversation::default());
        app.world_mut()
            .resource_mut::<rigcoder::Conversation>()
            .runs = 1;
        rig_ecs::bus::EffectLogResource::install(app.world_mut(), Default::default());
        app.world_mut().insert_resource(EffectLogOut {
            path: Some(scratch("write-error")),
            written: false,
        });
        write_effect_log(app.world_mut());
        assert!(matches!(app.should_exit(), Some(AppExit::Error(_))));
    }

    #[test]
    fn effect_log_is_published_as_a_complete_file() {
        let dir = scratch("atomic");
        let path = dir.join("effects.json");
        std::fs::write(&path, "old log").unwrap();
        write_log_atomically(&path, br#"{"records":[]}"#).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"records":[]}"#);
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1);
    }
}
