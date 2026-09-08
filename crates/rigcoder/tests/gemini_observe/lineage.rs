//! Matrix E — lineage, comparison and bounds (`observe_lineage`).
//!
//! Every cell but `two_runs` replays another matrix's recording, so the
//! same program runs under a different sink or policy and the traces are
//! compared. Record the other matrices first (`turns`, `gates`).
//!
//! | cell | dimension pinned | oracle | facts asserted | status |
//! |---|---|---|---|---|
//! | `disabled_witness` | the same cassette with no witness installed | transcript, effect-log records, delivery partitions, workspace file and request sequence identical to the witnessed replay | (no trace) | replay of `one_tool_stream` |
//! | `passes_a`, `passes_b` | two unwitnessed executions | records identical; pass numbers reported (the tool preparation runs on the task pool, so they may differ) | — | replay of `one_tool_stream` |
//! | `compare_equal` | two replays of one program, each under its own counting clock | both settled | `compare(a, b) == Equal` and the same facts (measurements are not compared: `clock` shows a clockless and a clocked trace `Equal`) | replay of `one_tool_stream` |
//! | `compare_diverged` | the same program under concurrency 1 instead of 4 | four results either way | `compare` → `Diverged { index }` at a `Gate`-stage fact, before any provider exchange differs | replay of `batch_c4_stream` |
//! | `compare_incomparable` | a sink of capacity 2 | run unaffected: settled, same answer | `dropped > 0`, `!is_complete()`, `compare` → `Incomparable { incomplete_actual }` | replay of `text_stream` |
//! | `expected_traces` | committed expected traces for three cells | — | `compare(expected, replayed) == Equal` against `fixtures/observe/<cell>.expected.json` (written in record mode) | replay of `text_stream`, `one_tool_stream`, `invalid_tool_stream` |
//! | `clock` | a counting host clock | settled | every `at` is `Some` and strictly increasing; `compare` with the clockless trace is `Equal` | replay of `text_stream` |
//! | `session` | `with_session("rigcoder/run/1")` configured by the cell | settled | the trace carries the configured session; `compare` ignores it (a renamed copy is `Equal`) | replay of `text_stream` |
//! | `correlation` | subjects on a four-call batch | four results | every tool `landed` effect id is a log record and every record landed; keys `tool:bash`, family Tool; four contiguous dispatch orders; completions' `order` increases; runtime-made tool effects carry no `parent` | replay of `batch_c4_stream` |
//! | `two_runs` | two runs in one session (the product's retry budget on, so a transient failure during recording would be part of the record; this recording holds two clean exchanges) | second request carries the first's history; both settled | scopes `rigcoder/run/1` then `rigcoder/run/2`, each ending `settled` | recorded |

use crate::support::*;
use rig::observe::{Action, Comparison, ObservationTrace, Stage, compare};

const MATRIX: &str = "observe_lineage";

fn one_tool(
    name: &str,
    witness: Option<WitnessConfig>,
) -> (
    Vec<rigcoder::Event>,
    rigcoder::EffectLog,
    String,
    Option<ObservationTrace>,
) {
    let mut out = None;
    run(
        MATRIX,
        name,
        Config {
            source: Source::of("observe_turns", "one_tool_stream"),
            witness: witness.clone(),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Run this command: printf hi > out.txt");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            let file = std::fs::read_to_string(cell.dir.join("out.txt")).unwrap();
            let trace = witness.as_ref().map(|_| cell.trace());
            out = Some((cell.events(), cell.log(), file, trace));
        },
    );
    out.unwrap()
}

#[test]
fn a_disabled_witness_changes_nothing() {
    let (events, log, file, none) = one_tool("disabled_witness", None);
    assert!(none.is_none());
    let (events2, log2, file2, some) = one_tool("enabled_witness", Some(WitnessConfig::default()));
    assert!(some.is_some());
    assert_eq!(file, "hi");
    assert_eq!(file, file2);
    assert_eq!(
        serde_json::to_value(&events).unwrap(),
        serde_json::to_value(&events2).unwrap(),
        "the transcript"
    );
    assert_eq!(
        serde_json::to_value(&log.records).unwrap(),
        serde_json::to_value(&log2.records).unwrap(),
        "the effect log's records"
    );
    // Delivery partitions: the same effects, the same transitions, the
    // same order. The pass number (`batch`) a delivery landed in is not
    // compared: the tool's preparation runs on the task pool, so two
    // unwitnessed runs disagree on it too (see `passes_vary_without_a_witness`).
    let partitions = |log: &rigcoder::EffectLog| -> Vec<serde_json::Value> {
        log.header
            .deliveries
            .iter()
            .flatten()
            .map(|d| serde_json::json!({"id": d.id, "kind": d.kind}))
            .collect()
    };
    assert_eq!(
        partitions(&log),
        partitions(&log2),
        "the delivery partitions"
    );
}

/// The evidence for not comparing pass numbers above: two unwitnessed
/// executions of the same recording, and whether their pass numbers agree.
#[test]
fn passes_vary_without_a_witness() {
    let (_, a, _, _) = one_tool("passes_a", None);
    let (_, b, _, _) = one_tool("passes_b", None);
    let batches = |log: &rigcoder::EffectLog| -> Vec<u64> {
        log.header
            .deliveries
            .iter()
            .flatten()
            .map(|d| d.batch)
            .collect()
    };
    eprintln!("[{MATRIX}/passes] a {:?} b {:?}", batches(&a), batches(&b));
    assert_eq!(
        serde_json::to_value(&a.records).unwrap(),
        serde_json::to_value(&b.records).unwrap()
    );
}

#[test]
fn two_executions_compare_equal() {
    let (_, _, _, a) = one_tool(
        "compare_equal_a",
        Some(WitnessConfig {
            clock: true,
            ..Default::default()
        }),
    );
    let (_, _, _, b) = one_tool(
        "compare_equal_b",
        Some(WitnessConfig {
            clock: true,
            ..Default::default()
        }),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(a.observations.iter().all(|o| o.at.is_some()));
    assert_eq!(compare(&a, &b), Comparison::Equal);
    // The semantic fields agree; a run's exact `at` stamps are its own.
    assert_eq!(facts(&a), facts(&b));
}

fn batch(name: &str, concurrency: usize) -> ObservationTrace {
    let mut out = None;
    run(
        MATRIX,
        name,
        Config {
            source: Source::of("observe_turns", "batch_c4_stream"),
            concurrency: Some(concurrency),
            ..Config::streamed()
        },
        |cell| {
            cell.submit(
                "Run these four commands, each as its own bash call, all in this one reply: echo 1 ; echo 2 ; echo 3 ; echo 4 (four separate calls, one command each)",
            );
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert_eq!(cell.tool_results(), 4);
            out = Some(cell.trace());
        },
    );
    out.unwrap()
}

#[test]
fn a_policy_change_diverges_at_the_gate() {
    let recorded = batch("compare_diverged_c4", 4);
    let changed = batch("compare_diverged_c1", 1);
    let Comparison::Diverged(divergence) = compare(&recorded, &changed) else {
        panic!("{:?}", compare(&recorded, &changed));
    };
    let expected = divergence
        .expected
        .as_ref()
        .expect("both have a fact there");
    let actual = divergence.actual.as_ref().expect("both have a fact there");
    eprintln!(
        "[{MATRIX}/compare_diverged] index {}: expected {:?} / actual {:?}",
        divergence.index, expected.action, actual.action
    );
    // The first disagreement is a decision, not an exchange: the batch
    // release under concurrency 1 keeps holds the recorded run released.
    assert!(
        expected.stage == Stage::Gate || actual.stage == Stage::Gate,
        "{divergence:?}"
    );
    let first_exchange_disagreement = recorded
        .observations
        .iter()
        .zip(&changed.observations)
        .position(|(a, b)| {
            matches!(a.action, Action::Issued | Action::Landed { .. })
                && (a.subject.key != b.subject.key
                    || std::mem::discriminant(&a.action) != std::mem::discriminant(&b.action))
        });
    assert!(
        first_exchange_disagreement.is_none_or(|i| i > divergence.index),
        "{first_exchange_disagreement:?}"
    );
}

#[test]
fn a_full_sink_is_incomparable_and_harmless() {
    let mut whole = None;
    run(
        MATRIX,
        "compare_incomparable_whole",
        Config {
            source: Source::of("observe_turns", "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            whole = Some((cell.answer(), cell.trace()));
        },
    );
    let (answer, whole) = whole.unwrap();
    run(
        MATRIX,
        "compare_incomparable",
        Config {
            source: Source::of("observe_turns", "text_stream"),
            witness: Some(WitnessConfig {
                capacity: Some(2),
                ..Default::default()
            }),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            assert_eq!(cell.ending(), "settled");
            assert_eq!(cell.answer(), answer, "the run is unaffected");
            let trace = rigcoder::observations(cell.app.world()).unwrap();
            assert_eq!(trace.observations.len(), 2);
            assert_eq!(trace.dropped, 1);
            assert!(!trace.is_complete());
            let Comparison::Incomparable { reason } = compare(&whole, &trace) else {
                panic!("{:?}", compare(&whole, &trace));
            };
            assert_eq!(reason.code, "incomplete_actual");
            let Comparison::Incomparable { reason } = compare(&trace, &whole) else {
                panic!()
            };
            assert_eq!(reason.code, "incomplete_expected");
        },
    );
}

fn expected_path(cell: &str) -> std::path::PathBuf {
    fixture_root()
        .join("observe")
        .join(format!("{cell}.expected.json"))
}

/// Committed expected traces: written in record mode, compared otherwise.
/// Semantic fields only — `at` is never stamped here and the session is
/// ignored by `compare`.
#[test]
fn committed_expected_traces() {
    for (source, prompt, config) in [
        (
            "text_stream",
            "Reply with the single word: pong",
            Config::streamed(),
        ),
        (
            "one_tool_stream",
            "Run this command: printf hi > out.txt",
            Config::streamed(),
        ),
        (
            "invalid_tool_stream",
            "Call the teleport function now.",
            Config {
                prompt: "You are a test agent. You have a function named teleport that takes no arguments. Call it whenever the user asks, without any other text.",
                ..Config::streamed()
            },
        ),
    ] {
        let name = format!("expected_{source}");
        run(
            MATRIX,
            &name,
            Config {
                source: Source::of("observe_turns", source),
                ..config
            },
            |cell| {
                cell.submit(prompt);
                cell.drive();
                let trace = cell.trace();
                let path = expected_path(source);
                if cell.recording()
                    || std::env::var("RIG_PROVIDER_TEST_MODE").is_ok_and(|m| m == "record")
                {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, serde_json::to_string_pretty(&trace).unwrap()).unwrap();
                }
                let expected: ObservationTrace =
                    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| {
                        panic!("{}: {e}; record the matrices first", path.display())
                    }))
                    .unwrap();
                assert_eq!(
                    compare(&expected, &trace),
                    Comparison::Equal,
                    "{source}: {:?} vs {:?}",
                    facts(&expected),
                    facts(&trace)
                );
            },
        );
    }
}

#[test]
fn a_host_clock_stamps_every_fact_and_changes_nothing_else() {
    let mut clockless = None;
    run(
        MATRIX,
        "clockless",
        Config {
            source: Source::of("observe_turns", "text_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            clockless = Some(cell.trace());
        },
    );
    let clockless = clockless.unwrap();
    assert!(clockless.observations.iter().all(|o| o.at.is_none()));
    run(
        MATRIX,
        "clock",
        Config {
            source: Source::of("observe_turns", "text_stream"),
            witness: Some(WitnessConfig {
                clock: true,
                ..Default::default()
            }),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            let trace = cell.trace();
            let stamps: Vec<_> = trace
                .observations
                .iter()
                .map(|o| o.at.expect("stamped"))
                .collect();
            assert!(
                stamps.windows(2).all(|w| w[0] < w[1]),
                "monotonic: {stamps:?}"
            );
            assert_eq!(compare(&clockless, &trace), Comparison::Equal);
            assert_eq!(facts(&clockless), facts(&trace));
        },
    );
}

#[test]
fn a_session_name_is_lineage_not_semantics() {
    run(
        MATRIX,
        "session",
        Config {
            source: Source::of("observe_turns", "text_stream"),
            witness: Some(WitnessConfig {
                session: Some("rigcoder/run/1".into()),
                ..Default::default()
            }),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: pong");
            cell.drive();
            let trace = cell.trace();
            assert_eq!(trace.session.as_deref(), Some("rigcoder/run/1"));
            // `finalized` is the host's call at exit (the CLI's `--observations`).
            let mut renamed = trace.clone();
            renamed.session = Some("elsewhere".into());
            assert_eq!(compare(&trace, &renamed), Comparison::Equal);
            let json = serde_json::to_value(&trace).unwrap();
            assert_eq!(json["session"], "rigcoder/run/1");
        },
    );
}

#[test]
fn subjects_correlate_with_the_record() {
    let mut out = None;
    run(
        MATRIX,
        "correlation",
        Config {
            source: Source::of("observe_turns", "batch_c4_stream"),
            concurrency: Some(4),
            ..Config::streamed()
        },
        |cell| {
            cell.submit(
                "Run these four commands, each as its own bash call, all in this one reply: echo 1 ; echo 2 ; echo 3 ; echo 4 (four separate calls, one command each)",
            );
            cell.drive();
            assert_eq!(cell.tool_results(), 4);
            out = Some((cell.trace(), cell.log()));
        },
    );
    let (trace, log) = out.unwrap();
    let ids: Vec<_> = log.records.iter().map(|r| r.id).collect();
    let tools: Vec<_> = trace
        .observations
        .iter()
        .filter(|o| {
            matches!(o.action, Action::Landed { .. })
                && o.subject
                    .key
                    .as_ref()
                    .is_some_and(|k| k.as_str() == "tool:bash")
        })
        .collect();
    assert_eq!(tools.len(), 4);
    for tool in &tools {
        assert!(ids.contains(&tool.subject.effect.unwrap()), "{tool:?}");
        assert_eq!(tool.subject.family, Some(rig::effect::EffectFamily::Tool));
        assert_eq!(tool.subject.scope.as_deref(), Some("rigcoder/run/1"));
        assert!(
            tool.subject.parent.is_none(),
            "a runtime-made effect has no dispatching parent: {tool:?}"
        );
        assert!(tool.subject.order.is_some());
    }
    // Landings come in completion order under concurrency 4; the dispatch
    // orders are four distinct, contiguous values.
    let mut orders: Vec<u64> = tools.iter().map(|t| t.subject.order.unwrap()).collect();
    orders.sort_unstable();
    assert!(
        orders.windows(2).all(|w| w[1] == w[0] + 1),
        "contiguous dispatch orders: {orders:?}"
    );
    let completions: Vec<_> = trace
        .observations
        .iter()
        .filter(|o| {
            matches!(o.action, Action::Issued)
                && o.subject
                    .key
                    .as_ref()
                    .is_some_and(|k| k.as_str() == rigcoder::model::MODEL_KEY)
        })
        .map(|o| o.subject.order.unwrap())
        .collect();
    assert_eq!(completions.len(), 2);
    assert!(completions[0] < completions[1]);
    // Every issued effect is a record, every record was issued.
    let issued: std::collections::BTreeSet<_> = trace
        .observations
        .iter()
        .filter(|o| matches!(o.action, Action::Landed { .. }))
        .map(|o| o.subject.effect.unwrap())
        .collect();
    assert_eq!(issued, ids.iter().copied().collect());
}

#[test]
fn two_runs_in_one_session() {
    // The product's retry budget: a live 503 during the recording is
    // itself a recorded fact, not a reason to re-record.
    run(
        MATRIX,
        "two_runs",
        Config {
            retries: 3,
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: one");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            let after_first = cell.trace().observations.len();
            cell.submit("Now reply with the single word: two");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert!(
                cell.answer().to_lowercase().contains("two"),
                "{}",
                cell.answer()
            );
            let trace = cell.trace();
            let scopes: Vec<_> = trace
                .observations
                .iter()
                .map(|o| o.subject.scope.clone().unwrap())
                .collect();
            assert!(
                scopes[..after_first].iter().all(|s| s == "rigcoder/run/1"),
                "{scopes:?}"
            );
            assert!(
                scopes[after_first..].iter().all(|s| s == "rigcoder/run/2"),
                "{scopes:?}"
            );
            let facts = facts(&trace);
            eprintln!("[{MATRIX}/two_runs] facts: {facts:?}");
            assert_eq!(facts[after_first - 1], "ended:settled");
            assert_eq!(facts.last().map(String::as_str), Some("ended:settled"));
            assert_eq!(
                facts
                    .iter()
                    .filter(|f| f.as_str() == "ended:settled")
                    .count(),
                2
            );
            // The second run's exchange carries the first as history.
            let log = cell.log();
            let last = log
                .records
                .iter()
                .rev()
                .find(|r| r.outcome.is_ok())
                .unwrap();
            let rig::effect::EffectKind::Completion { request, .. } = &last.kind else {
                panic!()
            };
            assert!(
                serde_json::to_string(request)
                    .unwrap()
                    .contains("single word: one")
            );
        },
    );
}
