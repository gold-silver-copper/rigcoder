//! The tools, each a `ToolFn` handler entity keyed `tool:<name>`. A tool is
//! a name, a description the model reads, a JSON schema, and a plain
//! function of its arguments: the result is text the model reads back.
//! Errors are returned as text too, so the model can recover.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

use bevy_ecs::prelude::*;
use rig::{
    serve::adapters::ToolFn,
    tool::{ToolContext, ToolExecutionError, ToolOutput},
    wasm_compat::WasmBoxedFuture,
};
use rig_ecs::bus::Handlers;
use serde_json::{Value, json};

use crate::file_change::FileSource;

const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
const MAX_BASH_TIMEOUT_SECS: u64 = 600;

/// The tools, in the order granted to the agent and advertised in replay.
pub const NAMES: [&str; 6] = [
    "read_file",
    "write_file",
    "edit_file",
    "list_files",
    "grep",
    "bash",
];

/// Register every tool; the entities are returned in [`NAMES`] order.
pub fn register_all(handlers: &mut Handlers, root: &Path) -> Vec<Entity> {
    let root: Arc<PathBuf> = Arc::new(root.to_path_buf());
    let mut entities = Vec::new();
    let mut add =
        |name: &str, tool: ToolFn<_>| match handlers.register(rig::effect::tool_key(name), tool) {
            Ok(entity) => entities.push(entity),
            Err(report) => tracing::error!("could not register tool {name}: {report}"),
        };
    add("read_file", read_file(root.clone()));
    add("write_file", write_file());
    add("edit_file", edit_file());
    add("list_files", list_files(root.clone()));
    add("grep", grep(root.clone()));
    add("bash", bash(root));
    entities
}

type Callback = Box<
    dyn for<'a> Fn(
            &'a mut ToolContext,
            Value,
        ) -> WasmBoxedFuture<'a, Result<ToolOutput, ToolExecutionError>>
        + Send
        + Sync,
>;

/// A synchronous function of the arguments becomes the handler's callback;
/// it runs on the bus's IO task pool thread while the world keeps ticking.
fn tool(
    name: &str,
    description: &str,
    parameters: Value,
    run: impl Fn(Value) -> Result<String, String> + Send + Sync + 'static,
) -> ToolFn<Callback> {
    let callback: Callback = Box::new(move |_context, args| {
        let text = match run(args) {
            Ok(text) => text,
            Err(error) => format!("error: {error}"),
        };
        // Record the complete handler output; steer shapes history after recording.
        Box::pin(async move { Ok(ToolOutput::text(text)) })
    });
    ToolFn::new(name, description, parameters, callback)
}

/// A tool whose work is a future: nothing blocks the bus's task pool while
/// a child process runs.
fn tool_async<Fut>(
    name: &str,
    description: &str,
    parameters: Value,
    run: impl Fn(Value, Arc<crate::approval::Permit>) -> Fut + Send + Sync + 'static,
) -> ToolFn<Callback>
where
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    let callback: Callback = Box::new(move |context, args| {
        let permit = match crate::approval::Permit::take_bash(context, &args) {
            Ok(permit) => permit,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let work = run(args, permit);
        Box::pin(async move {
            let text = match work.await {
                Ok(text) => text,
                Err(error) => format!("error: {error}"),
            };
            Ok(ToolOutput::text(text))
        })
    });
    ToolFn::new(name, description, parameters, callback)
}

fn resolve(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument {key:?}"))
}

fn read_file(root: Arc<PathBuf>) -> ToolFn<Callback> {
    tool(
        "read_file",
        "Read a text file. Returns its lines numbered from 1. A file over 2000 lines read without offset/limit returns an outline (definitions with line numbers) instead; then read the ranges you need.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the workspace"},
                "offset": {"type": "integer", "description": "First line to return (1-based, default 1)"},
                "limit": {"type": "integer", "description": "Maximum lines to return (default 2000)"}
            },
            "required": ["path"]
        }),
        move |args| {
            let path = resolve(&root, str_arg(&args, "path")?);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let ranged = args.get("offset").is_some() || args.get("limit").is_some();
            let total = text.lines().count();
            if !ranged && total > OUTLINE_THRESHOLD_LINES {
                return Ok(outline(&path, &text, total));
            }
            let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(2000) as usize;
            let mut out = String::new();
            for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
                out.push_str(&format!("{:>6}\t{line}\n", index + 1));
            }
            if offset - 1 + limit < total {
                out.push_str(&format!(
                    "[{total} lines total; showing {offset}..{}]\n",
                    offset - 1 + limit
                ));
            }
            if out.is_empty() {
                out.push_str("(empty file)");
            }
            Ok(out)
        },
    )
}

fn write_file() -> ToolFn<Callback> {
    file_tool(
        "write_file",
        "Create or overwrite a file with the given content. Parent directories are created. New files are private; replacements preserve existing permissions. Read-only and hard-linked targets are refused.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        }),
    )
}

fn edit_file() -> ToolFn<Callback> {
    file_tool(
        "edit_file",
        "Replace an exact string in a file. old_string must match exactly once unless replace_all is true. Returns the lines around each replacement as they now read.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact text to find (include enough context to be unique)"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            },
            "required": ["path", "old_string", "new_string"]
        }),
    )
}

fn file_tool(name: &'static str, description: &str, parameters: Value) -> ToolFn<Callback> {
    let callback: Callback = Box::new(move |context, args| {
        let result =
            crate::approval::Permit::apply_file(context, name, &args).map(ToolOutput::text);
        Box::pin(async move { result })
    });
    ToolFn::new(name, description, parameters, callback)
}

pub(crate) async fn prepare_file(
    root: &Path,
    name: &str,
    args: Value,
) -> Result<crate::approval::PreparedOperation, String> {
    let path = resolve(root, str_arg(&args, "path")?);
    let source = FileSource::read(&path)
        .map_err(|error| format!("cannot prepare {}: {error}", path.display()))?;
    let (after, replacement) = if name == "write_file" {
        let content = str_arg(&args, "content")?;
        check_mutation_size(Some(content.len()))?;
        (content.to_owned(), None)
    } else {
        let old = str_arg(&args, "old_string")?;
        let new = str_arg(&args, "new_string")?;
        if old.is_empty() {
            return Err("old_string must not be empty".to_owned());
        }
        let before = source.text().map_err(|error| error.to_string())?;
        let count = before.matches(old).count();
        if count == 0 {
            return Err(format!("old_string not found in {}", path.display()));
        }
        if count > 1 && !args["replace_all"].as_bool().unwrap_or(false) {
            return Err(format!(
                "old_string occurs {count} times in {}; add context to make it unique or set replace_all",
                path.display()
            ));
        }
        // Bound expansion before allocating: replace_all can amplify a small
        // repetitive source into an arbitrarily large result.
        check_mutation_size(count.checked_mul(new.len()).and_then(|added| {
            before
                .len()
                .checked_sub(count.checked_mul(old.len())?)?
                .checked_add(added)
        }))?;
        (
            before.replace(old, new),
            Some((count, old.to_owned(), new.to_owned())),
        )
    };
    if after.len() as u64 > crate::file_change::MAX_FILE_BYTES {
        return Err("file mutation exceeds the 16 MiB limit".into());
    }
    let mut unformatted = false;
    let after = if source
        .path()
        .extension()
        .is_some_and(|extension| extension == "rs")
    {
        match format_rust(&after, find_rustfmt(std::env::var_os("PATH").as_deref())).await? {
            Some(formatted) => formatted,
            None => {
                unformatted = true;
                after
            }
        }
    } else {
        after
    };
    let receipt = if let Some((count, old, new)) = replacement {
        format!(
            "{count} replacement(s) in {}\n{}",
            path.display(),
            context_after_edit(
                source.text().map_err(|error| error.to_string())?,
                &after,
                &old,
                &new
            )
        )
    } else {
        format!("wrote {} bytes to {}", after.len(), path.display())
    };
    // A container without a Rust toolchain still gets its file: the edit is
    // written as authored and the receipt says formatting was skipped.
    let receipt = if unformatted {
        format!("{receipt}\n(rustfmt not found on PATH; written unformatted)")
    } else {
        receipt
    };
    Ok(crate::approval::PreparedOperation::File {
        change: Box::new(source.prepare(after.into_bytes())),
        receipt,
    })
}

fn check_mutation_size(size: Option<usize>) -> Result<(), String> {
    if size.is_none_or(|size| size as u64 > crate::file_change::MAX_FILE_BYTES) {
        return Err("file mutation exceeds the 16 MiB limit".into());
    }
    Ok(())
}

/// The `rustfmt` on an absolute entry of `path`, if any.
fn find_rustfmt(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    path.into_iter()
        .flat_map(|paths| std::env::split_paths(paths).collect::<Vec<_>>())
        .filter(|path| path.is_absolute())
        .map(|path| path.join("rustfmt"))
        .find(|path| path.is_file())
}

/// Format Rust source with `executable`. `Ok(None)` when there is no
/// formatter: an edit is never refused for the container's toolchain, only
/// for what the formatter rejects.
async fn format_rust(
    contents: &str,
    executable: Option<PathBuf>,
) -> Result<Option<String>, String> {
    // Format stdin into an owned scratch file. --emit stdout prevents a module
    // path in model-authored Rust from modifying a different workspace file.
    const MAX_FORMAT_BYTES: u64 = 16 * 1024 * 1024;
    if contents.len() as u64 > MAX_FORMAT_BYTES {
        return Err("Rust edit exceeds the 16 MiB formatting limit".into());
    }
    let Some(executable) = executable else {
        return Ok(None);
    };
    let scratch = tempfile::tempdir().map_err(|error| error.to_string())?;
    let input = scratch.path().join("input.rs");
    let output = scratch.path().join("output.rs");
    let config = scratch.path().join("rustfmt.toml");
    std::fs::write(&input, contents).map_err(|error| error.to_string())?;
    std::fs::write(&config, "edition = \"2024\"\n").map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command
        .args(["--emit", "stdout", "--edition", "2024", "--config-path"])
        .arg(&config)
        .current_dir(scratch.path())
        .env_clear()
        .stdin(std::fs::File::open(&input).map_err(|error| error.to_string())?)
        .stdout(std::fs::File::create(&output).map_err(|error| error.to_string())?)
        .stderr(Stdio::piped());
    for name in [
        "PATH",
        "HOME",
        "RUSTUP_HOME",
        "CARGO_HOME",
        "RUSTUP_TOOLCHAIN",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let result = run_command(command, Duration::from_secs(10)).await?;
    if !result.success {
        return Err(format!(
            "Rust formatting failed before approval: {}",
            result.text
        ));
    }
    if std::fs::metadata(&output)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_FORMAT_BYTES
    {
        return Err("formatted Rust exceeds the 16 MiB limit".into());
    }
    std::fs::read_to_string(output)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn list_files(root: Arc<PathBuf>) -> ToolFn<Callback> {
    tool(
        "list_files",
        "List files under a directory (recursively, honoring .gitignore). Directories end with '/'.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory (default: the workspace)"},
                "max_depth": {"type": "integer", "description": "Recursion depth (default 3)"},
                "limit": {"type": "integer", "description": "Maximum entries (default 500)"}
            }
        }),
        move |args| {
            let dir = args["path"]
                .as_str()
                .map(|p| resolve(&root, p))
                .unwrap_or_else(|| root.as_ref().clone());
            let max_depth = args["max_depth"].as_u64().unwrap_or(3) as usize;
            let limit = args["limit"].as_u64().unwrap_or(500) as usize;
            let mut out = String::new();
            let mut count = 0usize;
            let mut truncated = false;
            for entry in ignore::WalkBuilder::new(&dir)
                .max_depth(Some(max_depth))
                .hidden(false)
                .build()
                .flatten()
            {
                if entry.depth() == 0 {
                    continue;
                }
                if count >= limit {
                    truncated = true;
                    break;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&dir)
                    .unwrap_or(entry.path())
                    .display();
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                out.push_str(&format!("{rel}{}\n", if is_dir { "/" } else { "" }));
                count += 1;
            }
            if truncated {
                out.push_str(&format!("[stopped after {limit} entries]\n"));
            }
            if out.is_empty() {
                out.push_str("(no entries)");
            }
            Ok(out)
        },
    )
}

fn grep(root: Arc<PathBuf>) -> ToolFn<Callback> {
    tool(
        "grep",
        "Search file contents with a regular expression (Rust regex syntax). Returns path:line:text matches.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "File or directory to search (default: the workspace)"},
                "glob": {"type": "string", "description": "Only files matching this glob, e.g. *.rs"},
                "limit": {"type": "integer", "description": "Maximum matches (default 200)"}
            },
            "required": ["pattern"]
        }),
        move |args| {
            let pattern = str_arg(&args, "pattern")?;
            let regex = regex::Regex::new(pattern).map_err(|e| format!("bad pattern: {e}"))?;
            let dir = args["path"]
                .as_str()
                .map(|p| resolve(&root, p))
                .unwrap_or_else(|| root.as_ref().clone());
            let glob = match args["glob"].as_str() {
                Some(g) => Some(
                    globset::Glob::new(g)
                        .map_err(|e| format!("bad glob: {e}"))?
                        .compile_matcher(),
                ),
                None => None,
            };
            let limit = args["limit"].as_u64().unwrap_or(200) as usize;
            let mut out = String::new();
            let mut count = 0usize;
            'files: for entry in ignore::WalkBuilder::new(&dir)
                .hidden(false)
                .build()
                .flatten()
            {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                if let Some(glob) = &glob
                    && !glob.is_match(entry.file_name())
                {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                for (index, line) in text.lines().enumerate() {
                    if regex.is_match(line) {
                        if count >= limit {
                            out.push_str(&format!("[stopped after {limit} matches]\n"));
                            break 'files;
                        }
                        let rel = entry
                            .path()
                            .strip_prefix(&dir)
                            .unwrap_or(entry.path())
                            .display();
                        out.push_str(&format!("{rel}:{}:{line}\n", index + 1));
                        count += 1;
                    }
                }
            }
            if out.is_empty() {
                out.push_str("(no matches)");
            }
            Ok(out)
        },
    )
}

fn bash(root: Arc<PathBuf>) -> ToolFn<Callback> {
    tool_async(
        "bash",
        "Run a shell command with bash in the workspace directory and return its stdout, stderr and exit code. Non-interactive: stdin is closed. Killed after timeout_secs (default 120, max 600).",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_secs": {"type": "integer"}
            },
            "required": ["command"]
        }),
        move |args, permit| {
            let command = str_arg(&args, "command").map(str::to_owned);
            let timeout = Duration::from_secs(
                args["timeout_secs"]
                    .as_u64()
                    .unwrap_or(DEFAULT_BASH_TIMEOUT_SECS)
                    .clamp(1, MAX_BASH_TIMEOUT_SECS),
            );
            let root = root.clone();
            async move {
                let command = command?;
                run_shell_cancellable(&root, &command, timeout, Some(permit.cancellation())).await
            }
        },
    )
}

/// How much of each stream a bounded capture keeps: this many bytes from
/// the start and this many from the end, with a count of what fell between.
const CAPTURE_HEAD: usize = 12_000;
const CAPTURE_TAIL: usize = 12_000;

/// A stream drained as it arrives into a head and a tail, so a command that
/// prints gigabytes costs a bounded buffer and never stalls on a full pipe.
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    dropped: usize,
}

impl Capture {
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.head.len() < CAPTURE_HEAD {
                self.head.push(b);
            } else {
                if self.tail.len() == CAPTURE_TAIL {
                    self.tail.pop_front();
                    self.dropped += 1;
                }
                self.tail.push_back(b);
            }
        }
    }

    fn render(self) -> String {
        let head = String::from_utf8_lossy(&self.head).into_owned();
        if self.tail.is_empty() {
            return head;
        }
        let tail: Vec<u8> = self.tail.into_iter().collect();
        let tail = String::from_utf8_lossy(&tail);
        if self.dropped == 0 {
            format!("{head}{tail}")
        } else {
            format!("{head}\n[... {} bytes omitted ...]\n{tail}", self.dropped)
        }
    }
}

#[cfg(unix)]
fn drain(
    mut pipe: impl Read + std::os::fd::AsRawFd + Send + 'static,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Capture> {
    std::thread::spawn(move || {
        let mut capture = Capture::default();
        let mut buffer = [0u8; 8192];
        while !stop.load(Ordering::Acquire) {
            // Poll before reading so even a detached descendant holding a
            // pipe open cannot keep the reader alive past cancellation.
            let mut fd = libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut fd, 1, 25) };
            if ready == 0 {
                continue;
            }
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => capture.push(&buffer[..n]),
            }
        }
        capture
    })
}

#[cfg(not(unix))]
fn drain(
    mut pipe: impl Read + Send + 'static,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Capture> {
    std::thread::spawn(move || {
        let mut capture = Capture::default();
        let mut buffer = [0u8; 8192];
        while !stop.load(Ordering::Acquire) {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => capture.push(&buffer[..n]),
            }
        }
        capture
    })
}

/// Spawn bash in its own process group, drain both pipes into bounded
/// captures as they arrive, wait on a dedicated thread, kill the whole
/// group at the deadline, and hand the result back through a oneshot so
/// the caller's future never blocks a pool thread.
#[cfg(test)]
fn run_shell(
    root: &Path,
    command: &str,
    timeout: Duration,
) -> impl std::future::Future<Output = Result<String, String>> + Send {
    run_shell_cancellable(root, command, timeout, None)
}

fn run_shell_cancellable(
    root: &Path,
    command: &str,
    timeout: Duration,
    cancelled: Option<Arc<AtomicBool>>,
) -> impl std::future::Future<Output = Result<String, String>> + Send {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let work = run_command_cancellable(cmd, timeout, cancelled);
    async move { work.await.map(|result| result.text) }
}

pub(crate) struct CommandOutput {
    pub text: String,
    pub success: bool,
}

pub(crate) fn run_command(
    command: Command,
    timeout: Duration,
) -> impl std::future::Future<Output = Result<CommandOutput, String>> + Send {
    run_command_cancellable(command, timeout, None)
}

fn run_command_cancellable(
    command: Command,
    timeout: Duration,
    cancelled: Option<Arc<AtomicBool>>,
) -> impl std::future::Future<Output = Result<CommandOutput, String>> + Send {
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let result = run_command_blocking(command, timeout, || {
            sender.is_canceled()
                || cancelled
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Acquire))
        });
        let _ = sender.send(result);
    });
    async move {
        receiver
            .await
            .unwrap_or_else(|_| Err("the shell runner thread ended without a result".to_owned()))
    }
}

fn run_command_blocking(
    mut cmd: Command,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<CommandOutput, String> {
    if cancelled() {
        return Err("command cancelled before launch".to_owned());
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start command: {e}"))?;
    let stop_readers = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().map(|p| drain(p, stop_readers.clone()));
    let stderr = child.stderr.take().map(|p| drain(p, stop_readers.clone()));
    let started = Instant::now();
    let mut killed = false;
    let mut status = None;
    let mut wait_error = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(found) => status = found,
                Err(error) => wait_error = Some(format!("waiting for command: {error}")),
            }
        }
        let drained = stdout.as_ref().is_none_or(|t| t.is_finished())
            && stderr.as_ref().is_none_or(|t| t.is_finished());
        if status.is_some() && drained {
            break;
        }
        // A shell may exit while its children still own the pipes. Keep
        // the deadline alive until the entire captured command has finished.
        if started.elapsed() >= timeout || cancelled() || wait_error.is_some() {
            #[cfg(unix)]
            unsafe {
                kill(-(child.id() as i32), 9);
            }
            let _ = child.kill();
            let _ = child.wait();
            stop_readers.store(true, Ordering::Release);
            killed = true;
            status = None;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let stdout = stdout
        .and_then(|t| t.join().ok())
        .map(Capture::render)
        .unwrap_or_default();
    let stderr = stderr
        .and_then(|t| t.join().ok())
        .map(Capture::render)
        .unwrap_or_default();
    if let Some(error) = wait_error {
        return Err(error);
    }
    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str(&stdout);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr.is_empty() {
        out.push_str("--- stderr ---\n");
        out.push_str(&stderr);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if killed {
        out.push_str(&format!("[killed after {}s timeout]\n", timeout.as_secs()));
    }
    let code = status.and_then(|s| s.code());
    out.push_str(&format!(
        "[exit code: {}]",
        code.map_or("signal".to_owned(), |c| c.to_string())
    ));
    Ok(CommandOutput {
        text: out,
        success: !killed && status.is_some_and(|status| status.success()),
    })
}

/// Above this many lines, an unranged read returns an outline.
const OUTLINE_THRESHOLD_LINES: usize = 2000;
const OUTLINE_MAX_ENTRIES: usize = 300;

/// Top-level definitions with their line numbers: what a reader wants of a
/// big file before choosing a range. Cheap per-language line patterns.
fn outline(path: &Path, text: &str, total: usize) -> String {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let patterns: &[&str] = match ext {
        "rs" => &[
            r"^\s*(pub(\([^)]*\))?\s+)?(async\s+)?(unsafe\s+)?(fn|struct|enum|trait|impl|mod|const|static|type|macro_rules!)\b",
        ],
        "py" => &[r"^\s*(async\s+)?(def|class)\b"],
        "js" | "jsx" | "ts" | "tsx" | "mjs" => &[
            r"^\s*(export\s+)?(default\s+)?(async\s+)?(function|class|interface|type|enum)\b",
            r"^\s*(export\s+)?(const|let|var)\s+\w+\s*=\s*(async\s*)?(\(|function|\w+\s*=>)",
        ],
        "go" => &[r"^(func|type)\b"],
        "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" => &[
            r"^[A-Za-z_][\w:<>*&\s]*\s\**[A-Za-z_]\w*\s*\([^;]*$",
            r"^\s*(struct|class|enum|union|namespace|typedef)\b",
            r"^#define\s+\w+",
        ],
        "java" | "kt" | "scala" | "cs" => &[
            r"^\s*(public|private|protected|static|final|abstract|override|open|sealed|data|internal)\b.*\b(class|interface|enum|object|fun|void|[A-Z]\w*)\b.*[({]\s*$",
            r"^\s*(class|interface|enum|object|fun|def)\b",
        ],
        "rb" => &[r"^\s*(def|class|module)\b"],
        "sh" | "bash" => &[r"^\s*(function\s+)?[A-Za-z_]\w*\s*\(\)\s*\{?"],
        _ => &[
            r"^\s*(pub\s+)?(fn|def|class|function|struct|enum|trait|impl|interface|type|module|func)\b",
        ],
    };
    let regexes: Vec<regex::Regex> = patterns
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect();
    let mut out = format!(
        "{} has {total} lines; showing an outline. Read a range with offset and limit.\n",
        path.display()
    );
    let mut entries = 0;
    for (index, line) in text.lines().enumerate() {
        if regexes.iter().any(|r| r.is_match(line)) {
            let shown: String = line.trim_end().chars().take(160).collect();
            out.push_str(&format!("{:>6}\t{shown}\n", index + 1));
            entries += 1;
            if entries >= OUTLINE_MAX_ENTRIES {
                out.push_str(&format!(
                    "[outline stopped after {OUTLINE_MAX_ENTRIES} entries]\n"
                ));
                break;
            }
        }
    }
    if entries == 0 {
        out.push_str("(no definitions recognised; read ranges of the file)\n");
    }
    out
}

/// Five lines around each actual replacement, including deletions.
fn context_after_edit(before: &str, after: &str, _old: &str, _new: &str) -> String {
    if after.is_empty() {
        return "(file is now empty)\n".to_owned();
    }
    let lines: Vec<&str> = after.lines().collect();
    let mut out = String::new();
    let mut shown_until = 0usize;
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_millis(250))
        .diff_lines(before, after);
    let mut cursor = 0usize;
    let mut positions = Vec::new();
    for change in diff.iter_all_changes() {
        if change.tag() != similar::ChangeTag::Equal {
            positions.push(cursor);
            if positions.len() == 20 {
                break;
            }
        }
        if change.tag() != similar::ChangeTag::Delete {
            cursor += 1;
        }
    }
    for &line in positions.iter().take(20) {
        let end_line = line;
        let start = line.saturating_sub(5).max(shown_until);
        let stop = (end_line + 6).min(lines.len());
        if start >= stop {
            continue;
        }
        if !out.is_empty() {
            out.push_str("   ...\n");
        }
        for (index, text) in lines.iter().enumerate().take(stop).skip(start) {
            out.push_str(&format!("{:>6}\t{text}\n", index + 1));
        }
        shown_until = stop;
    }
    out
}

#[cfg(test)]
mod rewrite_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn replacement_expansion_and_oversized_sources_are_refused_before_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "a".repeat(1024 * 1024)).unwrap();
        let result = futures::executor::block_on(prepare_file(
            dir.path(),
            "edit_file",
            json!({
                "path": "file.txt", "old_string": "a", "new_string": "b".repeat(1024), "replace_all": true,
            }),
        ));
        assert!(result.err().unwrap().contains("16 MiB"));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024 * 1024);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(crate::file_change::MAX_FILE_BYTES + 1)
            .unwrap();
        let result = futures::executor::block_on(prepare_file(
            dir.path(),
            "write_file",
            json!({
                "path": "file.txt", "content": "small",
            }),
        ));
        assert!(result.err().unwrap().contains("16 MiB"));
        assert!(check_mutation_size(None).is_err());
    }

    #[test]
    fn a_huge_output_comes_back_bounded_with_head_and_tail() {
        let out = futures::executor::block_on(run_shell(
            Path::new("."),
            "yes abcdefghij | head -c 20000000",
            Duration::from_secs(60),
        ))
        .unwrap();
        assert!(
            out.len() < CAPTURE_HEAD + CAPTURE_TAIL + 200,
            "len {}",
            out.len()
        );
        assert!(out.starts_with("abcdefghij\n"));
        assert!(out.contains("bytes omitted"));
        assert!(out.ends_with("[exit code: 0]"));
    }

    #[test]
    fn a_slow_command_does_not_hold_up_a_fast_one_on_one_thread() {
        // Both futures are polled by one thread. If the slow command
        // blocked the thread, the fast one could not resolve before the
        // slow one's timeout.
        let fast_done: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
        let started = Instant::now();
        let slow = run_shell(Path::new("."), "sleep 3", Duration::from_secs(2));
        let fast = run_shell(Path::new("."), "echo hi", Duration::from_secs(5));
        let record = fast_done.clone();
        let fast = async move {
            let out = fast.await;
            *record.lock().unwrap() = Some(started.elapsed());
            out
        };
        let (slow_out, fast_out) = futures::executor::block_on(futures::future::join(slow, fast));
        assert!(fast_out.unwrap().starts_with("hi\n"));
        let when = fast_done.lock().unwrap().unwrap();
        assert!(
            when < Duration::from_millis(1500),
            "fast command resolved after {when:?}"
        );
        assert!(slow_out.unwrap().contains("[killed after 2s timeout]"));
    }

    #[test]
    fn a_pipeline_is_killed_as_a_group_at_the_timeout() {
        let start = Instant::now();
        let out = futures::executor::block_on(run_shell(
            Path::new("."),
            "sleep 10 | cat",
            Duration::from_secs(1),
        ))
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(out.contains("[killed after 1s timeout]"));
    }

    #[test]
    fn edits_report_the_surrounding_lines() {
        let after = (1..=30)
            .map(|i| {
                if i == 15 {
                    "let x = 2;".to_owned()
                } else {
                    format!("line {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let before = after.replace("let x = 2;", "let x = 1;");
        let ctx = context_after_edit(&before, &after, "let x = 1;", "let x = 2;");
        assert!(ctx.contains("    10\tline 10"));
        assert!(ctx.contains("    15\tlet x = 2;"));
        assert!(ctx.contains("    20\tline 20"));
        assert!(!ctx.contains("line 22"));
    }

    #[test]
    fn a_missing_rustfmt_writes_rust_unformatted_instead_of_refusing() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(find_rustfmt(Some(empty.path().as_os_str())), None);
        assert_eq!(find_rustfmt(None), None);
        let source = "fn main(){println!(\"hi\");}\n";
        let result = futures::executor::block_on(format_rust(source, None)).unwrap();
        assert_eq!(
            result, None,
            "no formatter means no formatting, not an error"
        );
    }

    #[test]
    fn a_present_rustfmt_still_formats() {
        let Some(rustfmt) = find_rustfmt(std::env::var_os("PATH").as_deref()) else {
            return;
        };
        let source = "fn main(){println!(\"hi\");}\n";
        let result = futures::executor::block_on(format_rust(source, Some(rustfmt))).unwrap();
        assert_eq!(
            result.as_deref(),
            Some("fn main() {\n    println!(\"hi\");\n}\n")
        );
    }

    #[test]
    fn a_big_rust_file_reads_as_an_outline() {
        let mut text = String::new();
        for i in 0..2500 {
            if i % 500 == 0 {
                text.push_str(&format!("pub fn function_{i}() {{}}\n"));
            } else {
                text.push_str("    let _ = 1;\n");
            }
        }
        let out = outline(Path::new("big.rs"), &text, 2500);
        assert!(out.contains("2500 lines"));
        assert_eq!(out.matches("pub fn function_").count(), 5);
        assert!(out.contains("  2001\tpub fn function_2000"));
    }

    #[test]
    fn context_tracks_actual_replacement_when_new_text_is_common() {
        let before = format!("{}old\nend\n", "same\n".repeat(100));
        let after = before.replace("old", "same");
        let context = context_after_edit(&before, &after, "old", "same");
        assert!(context.contains("   101\tsame"));
        assert!(!context.contains("     1\tsame"));
    }

    #[test]
    fn deletion_keeps_context_and_accounts_for_prior_replacements() {
        let before = "one\nremove\ntwo\nremove\nthree\n";
        let after = before.replace("remove\n", "");
        let context = context_after_edit(before, &after, "remove\n", "");
        assert!(context.contains("     2\ttwo"));
        assert!(context.contains("     3\tthree"));
        assert!(context_after_edit("all", "", "all", "").contains("empty"));
    }

    #[test]
    fn background_child_cannot_outlive_the_capture_deadline() {
        let start = Instant::now();
        let result = futures::executor::block_on(run_shell(
            Path::new("."),
            "sleep 30 &",
            Duration::from_millis(200),
        ))
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(result.contains("timeout"));
        assert!(!result.contains("[exit code: 0]"));
    }

    #[test]
    #[cfg(unix)]
    fn detached_descendant_cannot_keep_capture_open() {
        let started = Instant::now();
        let result = futures::executor::block_on(run_shell(
            Path::new("."),
            "python3 -c 'import subprocess; subprocess.Popen([\"sleep\", \"3\"], start_new_session=True)'",
            Duration::from_millis(200),
        )).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(result.contains("timeout"), "{result}");
    }

    #[test]
    fn dropping_shell_future_terminates_the_command_group() {
        let root = std::env::temp_dir().join(format!(
            "rigcoder-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let future = run_shell(
            &root,
            "echo started > started; sleep 1; echo late > late",
            Duration::from_secs(30),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("started").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(root.join("started").exists());
        drop(future);
        std::thread::sleep(Duration::from_millis(1200));
        assert!(!root.join("late").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
