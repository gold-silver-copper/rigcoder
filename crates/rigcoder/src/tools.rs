//! The tools, each a `ToolFn` handler entity keyed `tool:<name>`. A tool is
//! a name, a description the model reads, a JSON schema, and a plain
//! function of its arguments: the result is text the model reads back.
//! Errors are returned as text too, so the model can recover.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
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

/// Output longer than this is cut in the middle, keeping the head and tail.
const MAX_OUTPUT_CHARS: usize = 30_000;
const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
const MAX_BASH_TIMEOUT_SECS: u64 = 600;

/// Register every tool; the entities, in a stable order, are what the agent
/// is granted.
pub fn register_all(handlers: &mut Handlers, root: &Path) -> Vec<Entity> {
    let root: Arc<PathBuf> = Arc::new(root.to_path_buf());
    let mut entities = Vec::new();
    let mut add = |name: &str, tool: ToolFn<_>| {
        match handlers.register(rig::effect::tool_key(name), tool) {
            Ok(entity) => entities.push(entity),
            Err(report) => tracing::error!("could not register tool {name}: {report}"),
        }
    };
    add("read_file", read_file(root.clone()));
    add("write_file", write_file(root.clone()));
    add("edit_file", edit_file(root.clone()));
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
        Box::pin(async move { Ok(ToolOutput::text(truncate(text))) })
    });
    ToolFn::new(name, description, parameters, callback)
}

fn truncate(text: String) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        return text;
    }
    let half = MAX_OUTPUT_CHARS / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(half)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n\n[... output truncated in the middle ...]\n\n{tail}")
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
        "Read a text file. Returns its lines numbered from 1. Use offset and limit for large files.",
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
            let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(2000) as usize;
            let total = text.lines().count();
            let mut out = String::new();
            for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
                out.push_str(&format!("{:>6}\t{line}\n", index + 1));
            }
            if offset - 1 + limit < total {
                out.push_str(&format!("[{total} lines total; showing {offset}..{}]\n", offset - 1 + limit));
            }
            if out.is_empty() {
                out.push_str("(empty file)");
            }
            Ok(out)
        },
    )
}

fn write_file(root: Arc<PathBuf>) -> ToolFn<Callback> {
    tool(
        "write_file",
        "Create or overwrite a file with the given content. Parent directories are created.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        }),
        move |args| {
            let path = resolve(&root, str_arg(&args, "path")?);
            let content = str_arg(&args, "content")?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            std::fs::write(&path, content)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
        },
    )
}

fn edit_file(root: Arc<PathBuf>) -> ToolFn<Callback> {
    tool(
        "edit_file",
        "Replace an exact string in a file. old_string must match exactly once unless replace_all is true. Returns a unified diff of the change.",
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
        move |args| {
            let path = resolve(&root, str_arg(&args, "path")?);
            let old = str_arg(&args, "old_string")?;
            let new = str_arg(&args, "new_string")?;
            let replace_all = args["replace_all"].as_bool().unwrap_or(false);
            if old.is_empty() {
                return Err("old_string must not be empty".to_owned());
            }
            let before = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let count = before.matches(old).count();
            if count == 0 {
                return Err(format!("old_string not found in {}", path.display()));
            }
            if count > 1 && !replace_all {
                return Err(format!(
                    "old_string occurs {count} times in {}; add context to make it unique or set replace_all",
                    path.display()
                ));
            }
            let after = if replace_all {
                before.replace(old, new)
            } else {
                before.replacen(old, new, 1)
            };
            std::fs::write(&path, &after)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            let diff = similar::TextDiff::from_lines(&before, &after)
                .unified_diff()
                .context_radius(3)
                .header(&path.display().to_string(), &path.display().to_string())
                .to_string();
            Ok(format!("{count} replacement(s) in {}\n{diff}", path.display()))
        },
    )
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
            'files: for entry in ignore::WalkBuilder::new(&dir).hidden(false).build().flatten() {
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
    tool(
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
        move |args| {
            let command = str_arg(&args, "command")?;
            let timeout = Duration::from_secs(
                args["timeout_secs"]
                    .as_u64()
                    .unwrap_or(DEFAULT_BASH_TIMEOUT_SECS)
                    .clamp(1, MAX_BASH_TIMEOUT_SECS),
            );
            run_shell(&root, command, timeout)
        },
    )
}

/// Spawn, drain both pipes on threads, poll for exit, kill at the deadline.
fn run_shell(root: &Path, command: &str, timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("cannot start bash: {e}"))?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let started = Instant::now();
    let mut killed = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => {
                #[cfg(unix)]
                {
                    let pgid = child.id() as i32;
                    unsafe {
                        kill(-pgid, 9);
                    }
                }
                let _ = child.kill();
                killed = true;
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => return Err(format!("waiting for bash: {error}")),
        }
    };
    let stdout = stdout.map(|t| t.join().unwrap_or_default()).unwrap_or_default();
    let stderr = stderr.map(|t| t.join().unwrap_or_default()).unwrap_or_default();
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
    out.push_str(&format!("[exit code: {}]", code.map_or("signal".to_owned(), |c| c.to_string())));
    Ok(out)
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        String::from_utf8_lossy(&buffer).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_timeout_kills_process_group() {
        let start = Instant::now();
        let result = run_shell(
            Path::new("."),
            "sleep 10 | cat",
            Duration::from_secs(1),
        ).unwrap();
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(result.contains("[killed after 1s timeout]"));
    }
}
