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

/// A tool whose work is a future: nothing blocks the bus's task pool while
/// a child process runs.
fn tool_async<Fut>(
    name: &str,
    description: &str,
    parameters: Value,
    run: impl Fn(Value) -> Fut + Send + Sync + 'static,
) -> ToolFn<Callback>
where
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    let callback: Callback = Box::new(move |_context, args| {
        let work = run(args);
        Box::pin(async move {
            let text = match work.await {
                Ok(text) => text,
                Err(error) => format!("error: {error}"),
            };
            Ok(ToolOutput::text(truncate(text)))
        })
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
            Ok(format!("{count} replacement(s) in {}\n{}", path.display(), context_after_edit(&after, new)))
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
        move |args| {
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
                run_shell(&root, &command, timeout).await
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

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Capture> {
    std::thread::spawn(move || {
        let mut capture = Capture::default();
        let mut buffer = [0u8; 8192];
        loop {
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
fn run_shell(root: &Path, command: &str, timeout: Duration) -> impl std::future::Future<Output = Result<String, String>> + Send {
    let (sender, receiver) = futures::channel::oneshot::channel();
    let root = root.to_path_buf();
    let command = command.to_owned();
    std::thread::spawn(move || {
        let _ = sender.send(run_shell_blocking(&root, &command, timeout));
    });
    async move {
        receiver
            .await
            .unwrap_or_else(|_| Err("the shell runner thread ended without a result".to_owned()))
    }
}

fn run_shell_blocking(root: &Path, command: &str, timeout: Duration) -> Result<String, String> {
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
    let stdout = stdout.and_then(|t| t.join().ok()).map(Capture::render).unwrap_or_default();
    let stderr = stderr.and_then(|t| t.join().ok()).map(Capture::render).unwrap_or_default();
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


/// Above this many lines, an unranged read returns an outline.
const OUTLINE_THRESHOLD_LINES: usize = 2000;
const OUTLINE_MAX_ENTRIES: usize = 300;

/// Top-level definitions with their line numbers: what a reader wants of a
/// big file before choosing a range. Cheap per-language line patterns.
fn outline(path: &Path, text: &str, total: usize) -> String {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let patterns: &[&str] = match ext {
        "rs" => &[r"^\s*(pub(\([^)]*\))?\s+)?(async\s+)?(unsafe\s+)?(fn|struct|enum|trait|impl|mod|const|static|type|macro_rules!)\b"],
        "py" => &[r"^\s*(async\s+)?(def|class)\b"],
        "js" | "jsx" | "ts" | "tsx" | "mjs" => &[r"^\s*(export\s+)?(default\s+)?(async\s+)?(function|class|interface|type|enum)\b", r"^\s*(export\s+)?(const|let|var)\s+\w+\s*=\s*(async\s*)?(\(|function|\w+\s*=>)"],
        "go" => &[r"^(func|type)\b"],
        "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" => &[r"^[A-Za-z_][\w:<>*&\s]*\s\**[A-Za-z_]\w*\s*\([^;]*$", r"^\s*(struct|class|enum|union|namespace|typedef)\b", r"^#define\s+\w+"],
        "java" | "kt" | "scala" | "cs" => &[r"^\s*(public|private|protected|static|final|abstract|override|open|sealed|data|internal)\b.*\b(class|interface|enum|object|fun|void|[A-Z]\w*)\b.*[({]\s*$", r"^\s*(class|interface|enum|object|fun|def)\b"],
        "rb" => &[r"^\s*(def|class|module)\b"],
        "sh" | "bash" => &[r"^\s*(function\s+)?[A-Za-z_]\w*\s*\(\)\s*\{?"],
        _ => &[r"^\s*(pub\s+)?(fn|def|class|function|struct|enum|trait|impl|interface|type|module|func)\b"],
    };
    let regexes: Vec<regex::Regex> = patterns.iter().filter_map(|p| regex::Regex::new(p).ok()).collect();
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
                out.push_str(&format!("[outline stopped after {OUTLINE_MAX_ENTRIES} entries]\n"));
                break;
            }
        }
    }
    if entries == 0 {
        out.push_str("(no definitions recognised; read ranges of the file)\n");
    }
    out
}

/// Five lines around each place `new` now occurs in the edited text, with
/// line numbers, so the model sees the edit as the file reads.
fn context_after_edit(after: &str, new: &str) -> String {
    let lines: Vec<&str> = after.lines().collect();
    let mut out = String::new();
    let mut shown_until = 0usize;
    let mut positions: Vec<usize> = Vec::new();
    if new.is_empty() {
        return String::new();
    }
    let mut from = 0;
    while let Some(found) = after[from..].find(new) {
        positions.push(from + found);
        from += found + new.len().max(1);
    }
    for pos in positions.iter().take(20) {
        let line = after[..*pos].matches('\n').count();
        let end_line = line + new.matches('\n').count();
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
    fn a_huge_output_comes_back_bounded_with_head_and_tail() {
        let out = futures::executor::block_on(run_shell(Path::new("."), "yes abcdefghij | head -c 20000000", Duration::from_secs(60))).unwrap();
        assert!(out.len() < CAPTURE_HEAD + CAPTURE_TAIL + 200, "len {}", out.len());
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
        assert!(when < Duration::from_millis(1500), "fast command resolved after {when:?}");
        assert!(slow_out.unwrap().contains("[killed after 2s timeout]"));
    }

    #[test]
    fn a_pipeline_is_killed_as_a_group_at_the_timeout() {
        let start = Instant::now();
        let out = futures::executor::block_on(run_shell(Path::new("."), "sleep 10 | cat", Duration::from_secs(1))).unwrap();
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(out.contains("[killed after 1s timeout]"));
    }

    #[test]
    fn edits_report_the_surrounding_lines() {
        let after = (1..=30).map(|i| if i == 15 { "let x = 2;".to_owned() } else { format!("line {i}") }).collect::<Vec<_>>().join("\n");
        let ctx = context_after_edit(&after, "let x = 2;");
        assert!(ctx.contains("    10\tline 10"));
        assert!(ctx.contains("    15\tlet x = 2;"));
        assert!(ctx.contains("    20\tline 20"));
        assert!(!ctx.contains("line 22"));
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
}
