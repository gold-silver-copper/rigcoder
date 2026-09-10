//! Dedicated host supervisor for budgeted, network-disabled task execution.
use anyhow::{Context, Result, ensure};
use std::{
    io::Read,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub struct Relay {
    child: Child,
    stopped: bool,
}

fn await_ready(child: &mut Child, timeout: Duration) -> Result<()> {
    let mut output = child.stdout.take().context("relay readiness pipe")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut ready = [0; 6];
        let _ = sender.send(output.read_exact(&mut ready).map(|()| ready));
    });
    let ready = receiver
        .recv_timeout(timeout)
        .context("trial relay readiness deadline or reader failure")??;
    ensure!(&ready == b"ready\n", "invalid trial relay readiness");
    Ok(())
}

impl Relay {
    pub fn start(
        container: &str,
        budget: &Path,
        phase: &str,
        key: &str,
        timeout: u64,
        context: &Path,
    ) -> Result<Self> {
        ensure!(
            (1..=7200).contains(&timeout),
            "budgeted trial timeout must be at most 7200 seconds"
        );
        let modules = [
            (
                "artifact_score",
                include_str!("../../../harness/artifact_score.py"),
            ),
            (
                "artifact_capture",
                include_str!("../../../harness/artifact_capture.py"),
            ),
            (
                "gemini_budget",
                include_str!("../../../harness/gemini_budget.py"),
            ),
            (
                "gemini_usage",
                include_str!("../../../harness/gemini_usage.py"),
            ),
            (
                "gemini_dispatch",
                include_str!("../../../harness/gemini_dispatch.py"),
            ),
            (
                "gemini_gateway",
                include_str!("../../../harness/gemini_gateway.py"),
            ),
            (
                "gemini_relay",
                include_str!("../../../harness/gemini_relay.py"),
            ),
            (
                "gemini_trial_relay",
                include_str!("../../../harness/gemini_trial_relay.py"),
            ),
        ];
        let mut script = String::from("import sys, types\nsources = {}\n");
        for (name, source) in modules {
            script.push_str(&format!("sources[{name:?}] = {}\nm = types.ModuleType({name:?}); sys.modules[{name:?}] = m\nexec(sources[{name:?}], m.__dict__)\n", serde_json::to_string(source)?));
        }
        script.push_str("raise SystemExit(sys.modules['gemini_trial_relay'].entry(sources))\n");
        let child = Command::new("python3")
            .args(["-I", "-c", &script])
            .arg(container)
            .arg(budget)
            .arg(phase)
            .arg(timeout.to_string())
            .arg(context)
            .env("GEMINI_API_KEY", key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting trusted trial relay")?;
        let mut relay = Self {
            child,
            stopped: false,
        };
        if let Err(error) = await_ready(&mut relay.child, Duration::from_secs(timeout.min(35))) {
            relay.child.kill()?;
            relay.child.wait()?;
            relay.stopped = true;
            return Err(error);
        }
        Ok(relay)
    }

    pub fn stop(&mut self) -> Result<()> {
        self.child.stdin.take(); // Parent EOF closes admission and stops the container.
        let deadline = Instant::now() + Duration::from_secs(75);
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.stopped = true;
                ensure!(status.success(), "trial relay failed; no score is valid");
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                self.child.wait()?;
                self.stopped = true;
                anyhow::bail!("trial relay shutdown exceeded limit; no score is valid");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_supervisor_readiness_has_an_independent_deadline() {
        let mut child = Command::new("python3")
            .args(["-I", "-c", "import time; time.sleep(60)"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let result = await_ready(&mut child, Duration::from_millis(50));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
