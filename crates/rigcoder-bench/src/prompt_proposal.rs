//! Embedded trusted launcher for the isolated prompt lane.
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};

#[derive(serde::Deserialize)]
pub struct Proposal {
    pub prompt: String,
    pub note: Option<String>,
}

pub fn launch(
    binary: &Path,
    prompt: &str,
    report: &str,
    port: u16,
    evidence: &Path,
) -> Result<Proposal> {
    ensure!(
        prompt.len() <= 131_072 && report.len() <= 4_194_304,
        "prompt proposal inputs exceed limits"
    );
    // Import trusted sources from this executable, never from candidate files.
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
            "improver_sandbox",
            include_str!("../../../harness/improver_sandbox.py"),
        ),
        (
            "prompt_workspace",
            include_str!("../../../harness/prompt_workspace.py"),
        ),
        (
            "prompt_improve",
            include_str!("../../../harness/prompt_improve.py"),
        ),
    ];
    let mut script = String::from("import sys, types, json, os\n");
    for (name, source) in modules {
        script.push_str(&format!(
            "m = types.ModuleType({name:?}); sys.modules[{name:?}] = m\nexec({}, m.__dict__)\n",
            serde_json::to_string(source)?
        ));
    }
    script.push_str(r#"
try:
    request = json.loads(sys.stdin.buffer.read(5_000_001))
    result = sys.modules['prompt_improve'].launch(
        sys.argv[1], request['prompt'].encode('utf-8'), request['report'].encode('utf-8'),
        int(sys.argv[2]), os.environ['RIGCODER_GATEWAY_TOKEN'], sys.argv[3])
    print(json.dumps({key: value.decode('utf-8') if value is not None else None for key, value in result.items()}))
except BaseException:
    sys.stderr.write('isolated prompt proposal failed; inspect retained evidence\n')
    sys.exit(1)
"#);
    let request = serde_json::to_vec(&serde_json::json!({"prompt": prompt, "report": report}))?;
    ensure!(
        request.len() <= 5_000_000,
        "encoded proposal inputs exceed limits"
    );
    let mut child = Command::new("python3")
        .args(["-I", "-c", &script])
        .arg(binary)
        .arg(port.to_string())
        .arg(evidence)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting isolated prompt launcher")?;
    let sent = child
        .stdin
        .take()
        .context("launcher stdin")?
        .write_all(&request);
    // Always reap the trusted launcher, including a failed request write.
    let output = child.wait_with_output()?;
    sent.context("sending prompt proposal inputs")?;
    ensure!(
        output.status.success(),
        "isolated prompt proposal failed; inspect {}",
        evidence.display()
    );
    ensure!(
        output.stdout.len() <= 1_200_000,
        "proposal response exceeds limit"
    );
    let proposal: Proposal = serde_json::from_slice(&output.stdout)?;
    ensure!(
        proposal.prompt.len() <= 131_072
            && proposal.note.as_ref().is_none_or(|n| n.len() <= 65_536),
        "proposal exceeds limits"
    );
    Ok(proposal)
}
