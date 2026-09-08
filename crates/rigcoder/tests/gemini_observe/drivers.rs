//! Matrix G — the driver's own decisions and the runtime's retry
//! (`observe_driver`): the `Action` variants the product reaches only under
//! a serving policy or a lost handler.
//!
//! | cell | dimension pinned | oracle | facts asserted | status |
//! |---|---|---|---|---|
//! | `deferred_intake` | `ServingPolicy { command_capacity: 1 }` over a four-call batch | four results | `deferred:intake_bound` at `Dispatch` (emitter `rig-ecs/bus`), once per intent left behind, before its `issued` | replay of `observe_turns/calls4_c4_stream` |
//! | `deferred_serial` | `serial_per_handler: true` over the same batch | four results, served one at a time | `deferred:serial_key_busy` for calls behind the one in flight, each before its `issued` | replay of `observe_turns/calls4_c4_stream` |
//! | `refused_handler` | the bash handler despawned while its call waits for approval | no file written; the model is told the tool is unavailable; the run ends on the replay miss that follows (no recording holds the refusal's read-back) | `approval:approved` then `refused:handler_unavailable` at `Dispatch` keyed `tool:bash`, never issued, no record | derived from `observe_gates/ask_approve_stream` (first exchange only) |
//! | `retry_deliverable_stream` | a required file missing after a text-only answer | the file exists at the end; two exchanges | `retry` at `Runtime` with the feedback the model reads, then `issued`… `ended:settled` | recorded |
//! | `approved` | `Action::Approved` | — | no producer in this product: the library reserves it for a host policy that approves through `Witnessing::emit`; rigcoder names its approvals as `rigcoder/approval` host facts (matrix B) | none (documented) |
//! | `refused_reentrant`, `refused_ids_exhausted` | `Refused { reentrant \| ids_exhausted }` | — | unreachable from the product's configuration (no handler dispatches to its own key; ids are 64-bit); proven by Rig's `driver_refusals_and_deferrals_are_witnessed` | none (documented) |

use crate::support::*;
use rig::observe::{Action, Stage};

const MATRIX: &str = "observe_driver";

const BATCH: &str = "Run these four commands, each as its own bash call, all in this one reply: echo 1 ; echo 2 ; echo 3 ; echo 4 (four separate calls, one command each)";

fn deferrals(cell: &Cell, code: &str) -> Vec<rig::observe::Observation> {
    cell.trace()
        .observations
        .into_iter()
        .filter(|o| matches!(&o.action, Action::Deferred { reason } if reason.code == code))
        .collect()
}

#[test]
fn an_intake_bound_defers() {
    run(
        MATRIX,
        "deferred_intake",
        Config {
            source: Source::of("observe_turns", "calls4_c4_stream"),
            concurrency: Some(4),
            volatile: true,
            ..Config::streamed()
        },
        |cell| {
            cell.app
                .insert_resource(rig_ecs::bus::Policy(rig::serve::ServingPolicy {
                    command_capacity: 1,
                    ..Default::default()
                }));
            cell.submit(BATCH);
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert_eq!(cell.tool_results(), 4);
            let facts = cell.facts();
            eprintln!("[{MATRIX}/deferred_intake] facts: {facts:?}");
            let deferred = deferrals(cell, "intake_bound");
            assert!(!deferred.is_empty(), "{facts:?}");
            for d in &deferred {
                assert_eq!(d.stage, Stage::Dispatch);
                assert_eq!(d.emitter.name, "rig-ecs/bus");
                assert!(d.subject.effect.is_none(), "not yet issued: {d:?}");
                assert!(d.subject.order.is_some(), "correlated by order: {d:?}");
                // Each deferred intent is issued later, under the same order.
                let issued_later = cell.trace().observations.iter().any(|o| {
                    matches!(o.action, Action::Issued)
                        && o.seq > d.seq
                        && o.subject.order == d.subject.order
                });
                assert!(issued_later, "{d:?}");
            }
            // Once per intent left behind, not once per pass.
            let orders: std::collections::BTreeSet<_> =
                deferred.iter().map(|d| d.subject.order).collect();
            assert_eq!(orders.len(), deferred.len(), "{deferred:?}");
        },
    );
}

#[test]
fn a_serial_key_defers() {
    run(
        MATRIX,
        "deferred_serial",
        Config {
            source: Source::of("observe_turns", "calls4_c4_stream"),
            concurrency: Some(4),
            volatile: true,
            ..Config::streamed()
        },
        |cell| {
            cell.app
                .insert_resource(rig_ecs::bus::Policy(rig::serve::ServingPolicy {
                    serial_per_handler: true,
                    ..Default::default()
                }));
            cell.submit(BATCH);
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert_eq!(cell.tool_results(), 4);
            let facts = cell.facts();
            eprintln!("[{MATRIX}/deferred_serial] facts: {facts:?}");
            let deferred = deferrals(cell, "serial_key_busy");
            assert!(!deferred.is_empty(), "{facts:?}");
            assert!(
                deferred.iter().all(|d| d
                    .subject
                    .key
                    .as_ref()
                    .is_some_and(|k| k.as_str() == "tool:bash")),
                "{deferred:?}"
            );
            let Action::Deferred { reason } = &deferred[0].action else {
                unreachable!()
            };
            assert!(
                reason.detail.as_deref().unwrap_or("").contains("tool:bash"),
                "{reason:?}"
            );
            assert_eq!(
                cell.count("refused:reentrant"),
                0,
                "the runtime is not a handler: {facts:?}"
            );
        },
    );
}

/// The runtime checks its model before assembling a turn, so a lost model
/// fails the run with no intent for the driver to refuse; and a tool
/// despawned before the request leaves the advertised tools, so the
/// recording no longer matches. The driver's own refusal needs the call to
/// exist first: the call is held for approval, its handler is despawned
/// under the hold, the approval releases it, and the dispatcher finds no
/// bound handler.
#[test]
fn a_lost_tool_handler_is_refused() {
    let source = cassette_path("observe_gates", "ask_approve_stream");
    // Only the first exchange: after the refusal the model's next request
    // carries the refusal text, which no recording holds — the replay's
    // miss ends the run, and that miss is the recorded story's end.
    let derived = derive(&source, &cassette_path(MATRIX, "refused_handler"), |docs| {
        docs.truncate(1);
    });
    run(
        MATRIX,
        "refused_handler",
        Config {
            approval: rigcoder::approval::ApprovalMode::Ask,
            source: Source::derived(derived, "observe_gates", "ask_approve_stream"),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Run this command: printf yes > ask.txt");
            cell.drive_until("an approval is pending", |world| {
                !world
                    .resource::<rigcoder::approval::Approvals>()
                    .pending
                    .is_empty()
            });
            let tools = cell
                .app
                .world()
                .resource::<rigcoder::AgentHandle>()
                .tools
                .clone();
            let bash = tools
                .into_iter()
                .find(|t| {
                    cell.app
                        .world()
                        .get::<rig_ecs::bus::Bound>(*t)
                        .is_some_and(|b| b.key.as_str() == "tool:bash")
                })
                .expect("the bash handler entity");
            cell.app.world_mut().despawn(bash);
            cell.app
                .world_mut()
                .resource_mut::<rigcoder::approval::Approvals>()
                .approve_next();
            cell.drive();
            assert!(!cell.dir.join("ask.txt").exists(), "no handler ran");
            let facts = cell.facts();
            eprintln!(
                "[{MATRIX}/refused_handler] facts: {facts:?} ending: {}",
                cell.ending()
            );
            let refused = cell
                .find(|a| matches!(a, Action::Refused { .. }))
                .expect("the driver refused the call");
            assert_eq!(refused.stage, Stage::Dispatch);
            assert_eq!(refused.emitter.name, "rig-ecs/bus");
            assert!(
                refused
                    .subject
                    .key
                    .as_ref()
                    .is_some_and(|k| k.as_str() == "tool:bash"),
                "{refused:?}"
            );
            assert!(
                refused.subject.effect.is_none(),
                "never issued: {refused:?}"
            );
            let Action::Refused { reason } = &refused.action else {
                unreachable!()
            };
            assert_eq!(reason.code, "handler_unavailable");
            assert!(
                reason.detail.as_deref().unwrap_or("").contains("tool:bash"),
                "{reason:?}"
            );
            let approved = facts
                .iter()
                .position(|f| f == "rigcoder/approval:approved")
                .unwrap();
            let refused_at = facts
                .iter()
                .position(|f| f == "refused:handler_unavailable")
                .unwrap();
            assert!(
                approved < refused_at,
                "approved, then refused by the driver: {facts:?}"
            );
            // A refused intent is no record; the model was told.
            let log = cell.log();
            assert!(
                log.records
                    .iter()
                    .all(|r| r.key.as_str() == rigcoder::model::MODEL_KEY),
                "{log:?}"
            );
            assert!(
                cell.events().iter().any(|e| matches!(e, rigcoder::Event::ToolResult { ok: false, output, .. } if output.contains("tool:bash"))),
                "{:?}",
                cell.events()
            );
        },
    );
}

#[test]
fn a_missing_deliverable_retries_the_turn() {
    run(
        MATRIX,
        "retry_deliverable_stream",
        Config {
            steer: Some(|steer| {
                steer.deliverables = vec![std::path::PathBuf::from(
                    "/tmp/rigcoder-observe/observe_driver/retry_deliverable_stream/report.txt",
                )];
                steer.max_deliverable_retries = 2;
            }),
            prompt: "You are a test agent in a workspace. Answer the user directly. If told that required files are missing, create them with the bash tool (printf), then reply with one short sentence.",
            max_turns: 6,
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Say hello. Do not use any tool.");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert!(cell.dir.join("report.txt").exists(), "{:?}", cell.events());
            let facts = cell.facts();
            eprintln!("[{MATRIX}/retry_deliverable_stream] facts: {facts:?}");
            let retry = cell
                .find(|a| matches!(a, Action::Retry { .. }))
                .expect("the turn was retried");
            assert_eq!(retry.stage, Stage::Runtime);
            assert_eq!(retry.subject.scope.as_deref(), Some("rigcoder/run/1"));
            let Action::Retry { feedback } = retry.action else {
                unreachable!()
            };
            assert!(
                feedback.as_deref().unwrap_or("").contains("report.txt"),
                "{feedback:?}"
            );
            // The retry sits between the first landing and the next request.
            let first_landed = facts.iter().position(|f| f == "landed").unwrap();
            let retry_at = facts.iter().position(|f| f == "retry").unwrap();
            let second_issued = facts
                .iter()
                .enumerate()
                .filter(|(_, f)| f.as_str() == "issued")
                .nth(1)
                .map(|(i, _)| i)
                .unwrap();
            assert!(
                first_landed < retry_at && retry_at < second_issued,
                "{facts:?}"
            );
            // The model read the feedback: the second request carries it.
            let log = cell.log();
            let rig::effect::EffectKind::Completion { request, .. } = &log.records[1].kind else {
                panic!()
            };
            assert!(
                serde_json::to_string(request)
                    .unwrap()
                    .contains("required files do not exist")
            );
        },
    );
}
