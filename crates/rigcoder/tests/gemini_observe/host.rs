//! Matrix F — the host surface (`observe_host`).
//!
//! | cell | dimension pinned | oracle | facts asserted | status |
//! |---|---|---|---|---|
//! | `cli_source_stream` | a one-tool run under the CLI's default run settings | file written | (source of the next) | recorded |
//! | `cli_replay` | `rigcoder --replay <effects.json> --observations <out.json>` (the product binary, replay mode) | exit 0, the file exists | host-monotonic effect-log replay metadata, clock stamps and run timing; finalized, complete, settled, `issued`/`landed` per exchange | replay of `cli_source_stream` through the CLI |
//! | `digest` | `rigcoder-bench digest <job>` over a trial holding a recorded trace | `digest.json` written | `observed.stream_truncations == 1`, `provider_retries == 1`, `endings == [provider, settled]`, `complete` | replay of `observe_failures/stream_truncated_stream` |
//! | `host_kinds_round_trip` | the four `HostAction` kinds | — | each `action()` → `from_action` round-trips; a foreign kind is `None` | in-process |
//! | `invalid_submission` | rejected zero-token settings before a run exists | no dispatch or provider request | one matching structured transcript/host failure, absent scope/status/attempt | local-only |
//! | `stream_events_kept` | the effect log with and without kept stream events on a failed completion | — | `events` is `None` by default and `Some(non-empty)` when kept: the frames a failure needs (the first audited gap; the CLI keeps them whenever it writes an effect log) | replay of `observe_failures/stream_truncated_stream` |

use crate::support::*;
use rig::observe::{Action, HostAction as _, ObservationTrace};

const MATRIX: &str = "observe_host";

#[test]
fn a_rejected_submission_has_structured_evidence_without_a_provider_call() {
    run(
        MATRIX,
        "invalid_submission",
        Config {
            max_tokens: 0,
            source: Source::None,
            ..Config::unary()
        },
        |cell| {
            assert!(rigcoder::submit(cell.app.world_mut(), "never sent").is_none());
            assert_eq!(cell.log().records.len(), 0);
            let failure = cell.failure();
            assert_eq!(failure.kind, "invalid_configuration");
            assert!(failure.adapter.is_none());
            assert!(failure.http_status.is_none());
            rigcoder::observe::finalize(cell.app.world());
            let trace = cell.trace();
            assert_eq!(trace.observations.len(), 1);
            let observation = &trace.observations[0];
            assert!(observation.subject.scope.is_none());
            assert_eq!(
                rigcoder::failure::FailureDetail::from_action(&observation.action)
                    .unwrap()
                    .unwrap(),
                failure
            );
        },
    );
}

fn cli_prompt_file() -> std::path::PathBuf {
    let path = std::path::PathBuf::from("/tmp/rigcoder-observe").join("cli-prompt.txt");
    std::fs::write(&path, PROMPT).unwrap();
    path
}

#[test]
fn the_cli_writes_the_trace_of_a_replayed_run() {
    let mut recorded = None;
    run(
        MATRIX,
        "cli_source_stream",
        Config {
            max_tokens: 16_000,
            retries: 3,
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Run this command: printf cli > cli.txt");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert_eq!(
                std::fs::read_to_string(cell.dir.join("cli.txt")).unwrap(),
                "cli"
            );
            recorded = Some((cell.dir.clone(), cell.log(), cell.trace()));
        },
    );
    let (dir, log, in_process) = recorded.unwrap();
    let scratch = std::path::PathBuf::from("/tmp/rigcoder-observe").join("cli_replay");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let effects = scratch.join("effects.json");
    std::fs::write(&effects, serde_json::to_vec(&log).unwrap()).unwrap();
    let observations = scratch.join("observations.json");
    let output = std::process::Command::new(env!("CARGO"))
        .args(["run", "-q", "--locked", "-p", "rigcoder-cli", "--"])
        .arg("--replay")
        .arg(&effects)
        .arg("--observations")
        .arg(&observations)
        .arg("--prompt-file")
        .arg(cli_prompt_file())
        .arg("-C")
        .arg(&dir)
        // The replay identity covers the agent's spec: the same turn budget.
        .args(["--provider", "gemini", "--model", MODEL, "--max-turns", "4"])
        .current_dir(fixture_root().parent().unwrap())
        .env_remove("GEMINI_API_KEY")
        .output()
        .expect("the CLI runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit {:?}\n{stdout}\n{stderr}",
        output.status.code()
    );
    let artifact: rigcoder::observe::ObservationArtifact =
        serde_json::from_str(&std::fs::read_to_string(&observations).unwrap()).unwrap();
    assert_eq!(
        artifact.measurement_context,
        Some(rigcoder::observe::MeasurementContext {
            execution_mode: rigcoder::observe::ExecutionMode::EffectLogReplay,
            clock_source: rigcoder::observe::ClockSource::HostMonotonic,
        })
    );
    assert_eq!(
        artifact.recording_provenance,
        Some(rigcoder::observe::RecordingProvenance::NotApplicable)
    );
    let trace = artifact.trace;
    assert!(trace.observations.iter().all(|fact| fact.at.is_some()));
    assert!(trace.finalized, "the CLI finalizes at exit");
    assert!(trace.is_complete());
    let facts = facts(&trace);
    eprintln!("[{MATRIX}/cli_replay] facts: {facts:?}");
    assert_eq!(facts.last().map(String::as_str), Some("ended:settled"));
    let exchanges = trace
        .observations
        .iter()
        .filter(|o| matches!(o.action, Action::Issued))
        .count();
    assert_eq!(
        exchanges,
        log.records.len(),
        "one dispatch per recorded exchange"
    );
    let kinds: std::collections::BTreeSet<_> = trace
        .observations
        .iter()
        .filter_map(|o| match &o.action {
            Action::Host { kind, .. } => Some(kind.clone()),
            _ => None,
        })
        .collect();
    let declared: std::collections::BTreeSet<_> = [
        rigcoder::observe::Approval::KIND,
        rigcoder::observe::SteerDenial::KIND,
        rigcoder::observe::ResultShaped::KIND,
        rigcoder::observe::ProviderRetry::KIND,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert!(kinds.is_subset(&declared), "{kinds:?}");
    // Exact replay has no live gate: the exchanges and the ending are the
    // same story as in process, the host's approval facts are not.
    let exchanges_in_process: Vec<_> = crate::support::facts(&in_process)
        .into_iter()
        .filter(|f| f == "issued" || f == "landed" || f.starts_with("ended:"))
        .collect();
    let exchanges_cli: Vec<_> = facts
        .iter()
        .filter(|f| *f == "issued" || *f == "landed" || f.starts_with("ended:"))
        .cloned()
        .collect();
    assert_eq!(exchanges_in_process, exchanges_cli);
}

fn truncated_cell(
    name: &str,
    keep: bool,
) -> (
    Vec<rigcoder::Event>,
    rigcoder::EffectLog,
    rigcoder::observe::ObservationArtifact,
) {
    let derived = cassette_path("observe_failures", "stream_truncated_stream");
    assert!(
        derived.is_file(),
        "run observe_failures first: {}",
        derived.display()
    );
    let mut out = None;
    run(
        MATRIX,
        name,
        Config {
            retries: 3,
            keep_stream_events: keep,
            source: Source::derived(derived, "observe_turns", "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            assert_eq!(cell.ending(), "settled");
            rigcoder::observe::finalize(cell.app.world());
            out = Some((
                cell.events(),
                cell.log(),
                rigcoder::observe::artifact(cell.app.world()).unwrap(),
            ));
        },
    );
    out.unwrap()
}

#[test]
fn the_bench_digest_counts_the_trace() {
    let (events, _, trace) = truncated_cell("digest_source", false);
    let job = std::path::PathBuf::from("/tmp/rigcoder-observe").join("digest-job");
    let _ = std::fs::remove_dir_all(&job);
    let agent = job.join("pong__1").join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let transcript: String = events
        .iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect();
    std::fs::write(agent.join("transcript.jsonl"), transcript).unwrap();
    std::fs::write(
        agent.join("observations.json"),
        serde_json::to_string(&trace).unwrap(),
    )
    .unwrap();
    std::fs::write(
        job.join("pong__1").join("result.json"),
        r#"{"task": "pong", "attempt": 1, "reward": 0.0}"#,
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "-q",
            "--locked",
            "-p",
            "rigcoder-bench",
            "--",
            "digest",
        ])
        .arg(&job)
        .current_dir(fixture_root().parent().unwrap())
        .output()
        .expect("the bench runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let digest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(job.join("digest.json")).unwrap()).unwrap();
    let rendered = String::from_utf8_lossy(&output.stdout);
    eprintln!("[{MATRIX}/digest]\n{rendered}");
    let trial = digest["failed"]
        .as_array()
        .or_else(|| digest["trials"].as_array())
        .and_then(|t| t.iter().find(|t| t["trial"] == "pong__1"))
        .unwrap_or_else(|| panic!("{digest}"));
    let observed = &trial["observed"];
    assert_eq!(trial["task"], "pong");
    assert_eq!(trial["task_source"], "result_file");
    assert_eq!(trial["attempt"], 1);
    assert!(digest.get("evaluation").unwrap().is_null());
    assert_eq!(observed["recording_provenance"], "derived");
    assert_eq!(
        observed["measurement_context"],
        serde_json::json!({
            "execution_mode": "cassette_replay", "clock_source": "absent",
        })
    );
    assert_eq!(observed["complete"], true, "{observed}");
    assert_eq!(observed["stream_truncations"], 1, "{observed}");
    assert_eq!(observed["provider_retries"], 1, "{observed}");
    assert_eq!(
        serde_json::Value::Array(
            observed["endings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|run| run["ending"].clone())
                .collect()
        ),
        serde_json::json!(["provider", "settled"]),
        "{observed}"
    );
    assert!(rendered.contains("truncat"), "{rendered}");
}

#[test]
fn host_kinds_round_trip() {
    let approval = rigcoder::observe::Approval {
        operation: "op".into(),
        tool: "bash".into(),
        mode: "ask".into(),
        decision: "denied".into(),
        reason: Some("no".into()),
    };
    let steer = rigcoder::observe::SteerDenial {
        tool: "bash".into(),
        rule: "deny".into(),
        reason: "because".into(),
    };
    let shaped = rigcoder::observe::ResultShaped {
        tool: "read_file".into(),
        chars: 40_000,
        kept: 30_000,
    };
    let retry = rigcoder::observe::ProviderRetry {
        operation: None,
        attempt: 1,
        wait_secs: 2,
        reason: "timed out".into(),
    };
    let a = approval.action().unwrap();
    let s = steer.action().unwrap();
    let h = shaped.action().unwrap();
    let r = retry.action().unwrap();
    for (action, kind) in [
        (&a, "rigcoder/approval"),
        (&s, "rigcoder/steer"),
        (&h, "rigcoder/result_shaped"),
        (&r, "rigcoder/provider_retry"),
    ] {
        let Action::Host { kind: k, payload } = action else {
            panic!()
        };
        assert_eq!(k, kind);
        assert!(payload.is_object());
    }
    assert_eq!(
        rigcoder::observe::Approval::from_action(&a)
            .unwrap()
            .unwrap(),
        approval
    );
    assert_eq!(
        rigcoder::observe::SteerDenial::from_action(&s)
            .unwrap()
            .unwrap(),
        steer
    );
    assert_eq!(
        rigcoder::observe::ResultShaped::from_action(&h)
            .unwrap()
            .unwrap(),
        shaped
    );
    assert_eq!(
        rigcoder::observe::ProviderRetry::from_action(&r)
            .unwrap()
            .unwrap(),
        retry
    );
    assert!(
        rigcoder::observe::Approval::from_action(&s).is_none(),
        "a foreign kind is not this fact"
    );
    assert!(rigcoder::observe::ProviderRetry::from_action(&Action::Issued).is_none());
    // Through a serialized trace, the same.
    let trace = ObservationTrace {
        session: None,
        observations: vec![rig::observe::Observation::new(
            rig::observe::Subject::scoped("rigcoder/run/1"),
            rig::observe::Stage::Host,
            rigcoder::observe::emitter("approval"),
            a.clone(),
        )],
        dropped: 0,
        finalized: true,
    };
    let back: ObservationTrace =
        serde_json::from_str(&serde_json::to_string(&trace).unwrap()).unwrap();
    assert_eq!(
        rigcoder::observe::Approval::from_action(&back.observations[0].action)
            .unwrap()
            .unwrap(),
        approval
    );
    assert_eq!(back.observations[0].emitter.name, "rigcoder/approval");
    assert_eq!(
        back.observations[0].emitter.version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn stream_events_are_kept_when_asked() {
    let (_, folded, _) = truncated_cell("stream_events_folded", false);
    let failed = folded
        .records
        .iter()
        .find(|r| r.outcome.is_err())
        .expect("the cut completion");
    assert!(
        failed.events.is_none(),
        "the default folds: {:?}",
        failed.events
    );
    let (_, kept, _) = truncated_cell("stream_events_kept", true);
    let failed = kept
        .records
        .iter()
        .find(|r| r.outcome.is_err())
        .expect("the cut completion");
    let events = failed.events.as_ref().expect("kept");
    assert!(!events.is_empty(), "the frames the failure needs");
    eprintln!(
        "[{MATRIX}/stream_events_kept] {} events kept on the failed completion",
        events.len()
    );
    let settled = kept.records.iter().find(|r| r.outcome.is_ok()).unwrap();
    assert!(settled.events.as_ref().is_some_and(|e| !e.is_empty()));
}
