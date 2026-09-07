//! Task slices: `harness/slices/<name>.txt`, one task per line, `#` comments.

use std::path::Path;

use anyhow::{Context, Result, bail};

pub fn read(root: &Path, name: &str) -> Result<Vec<String>> {
    let path = root
        .join("harness")
        .join("slices")
        .join(format!("{name}.txt"));
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let tasks: Vec<String> = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    if tasks.is_empty() {
        bail!("{} names no tasks", path.display());
    }
    Ok(tasks)
}
