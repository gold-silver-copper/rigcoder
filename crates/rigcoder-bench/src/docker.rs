//! The `docker` CLI, as functions. `DOCKER_HOST` and `DOCKER_CONFIG` come
//! from the environment, as they do for the CLI itself.

use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn docker() -> Command {
    Command::new("docker")
}

fn run(cmd: &mut Command, what: &str) -> Result<Output> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running docker for {what}"))?;
    Ok(Output {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

pub fn available() -> Result<()> {
    host_timeout()?;
    let out = run(
        docker()
            .arg("version")
            .arg("--format")
            .arg("{{.Server.Version}}"),
        "version",
    )?;
    if out.code != 0 {
        bail!("docker daemon unreachable: {}", out.stderr.trim());
    }
    Ok(())
}

/// Homebrew installs GNU coreutils as `gtimeout` on macOS. Check for the
/// required implementation before starting a job, not midway through builds.
fn host_timeout() -> Result<&'static str> {
    for executable in ["timeout", "gtimeout"] {
        if let Ok(output) = Command::new(executable).arg("--version").output()
            && output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("GNU coreutils")
        {
            return Ok(executable);
        }
    }
    bail!(
        "GNU timeout is required for image builds: install coreutils (on macOS: brew install coreutils) and put timeout or gtimeout on PATH"
    )
}

/// Build the task image from `environment/`, natively for this daemon.
pub fn build(context: &Path, tag: &str, timeout: Duration) -> Result<Output> {
    let mut cmd = Command::new(host_timeout()?);
    cmd.arg("--signal=KILL")
        .arg(timeout.as_secs().to_string())
        .arg("docker")
        .arg("build")
        .arg("-t")
        .arg(tag)
        .arg(context);
    run(&mut cmd, "build")
}

/// Start a container that idles until removed.
pub fn start(name: &str, image: &str, workdir: &str, cpus: f64, memory: &str) -> Result<()> {
    let mut cmd = docker();
    cmd.args([
        "run",
        "-d",
        "--name",
        name,
        "--cpus",
        &cpus.to_string(),
        "--memory",
        memory,
        "-w",
        workdir,
    ])
    .args([
        "--entrypoint",
        "sh",
        image,
        "-c",
        "while true; do sleep 3600; done",
    ]);
    let out = run(&mut cmd, "run")?;
    if out.code != 0 {
        bail!("docker run failed: {}", out.stderr.trim());
    }
    Ok(())
}

pub fn remove(name: &str) {
    let _ = run(docker().args(["rm", "-f", name]), "rm");
}

pub fn copy_in(name: &str, from: &Path, to: &str) -> Result<()> {
    let out = run(
        docker().arg("cp").arg(from).arg(format!("{name}:{to}")),
        "cp in",
    )?;
    if out.code != 0 {
        bail!(
            "docker cp {} -> {to}: {}",
            from.display(),
            out.stderr.trim()
        );
    }
    Ok(())
}

pub fn copy_out(name: &str, from: &str, to: &Path) -> Result<()> {
    let out = run(
        docker().arg("cp").arg(format!("{name}:{from}")).arg(to),
        "cp out",
    )?;
    if out.code != 0 {
        bail!(
            "docker cp {from} -> {}: {}",
            to.display(),
            out.stderr.trim()
        );
    }
    Ok(())
}

/// Run a shell command in the container as root, with a hard timeout
/// enforced inside the container (coreutils `timeout`, present in every
/// base image the dataset uses), and `env` exported to it.
pub fn exec(
    name: &str,
    workdir: &str,
    env: &[(&str, &str)],
    script: &str,
    timeout: Duration,
) -> Result<Output> {
    let mut cmd = docker();
    cmd.args(["exec", "-w", workdir]);
    for (key, value) in env {
        cmd.arg("-e").arg(format!("{key}={value}"));
    }
    cmd.arg(name).args([
        "timeout",
        "--signal=KILL",
        &timeout.as_secs().to_string(),
        "bash",
        "-c",
        script,
    ]);
    run(&mut cmd, "exec")
}

/// The file's contents, or `None` if it does not exist.
pub fn read_file(name: &str, path: &str) -> Result<Option<String>> {
    let out = run(docker().args(["exec", name, "cat", path]), "cat")?;
    Ok((out.code == 0).then_some(out.stdout))
}
