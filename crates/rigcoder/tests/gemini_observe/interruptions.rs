//! Matrix C — interruptions (`observe_interruptions`).
//!
//! | cell | dimension pinned | oracle | facts asserted | status |
//! |---|---|---|---|---|
//! | `cancel_before_dispatch` | cancel in the frame the run was submitted | no request made; run failed cancelled | exactly `cancel_requested`, `ended:cancelled` (no intent had been assembled yet) | no-wire |
//! | `long_answer_stream` | a long streamed answer, whole | settled | `issued, landed, ended:settled` | recorded (source of the next) |
//! | `cancel_mid_stream` | cancel after the first text delta landed | run failed with the operator's reason; the completion lands whole after the run ended and no history reads it | `issued`, `cancel_requested`, `ended:cancelled`, then the model's `landed` (the product keeps the run entity, so the bus never drops the dispatch: no `cancelled@Collect`) | derived: the recorded frames served one per 150 ms by a local pacing server (an instant replay lands the whole stream in one pass) |
//! | `cancel_mid_tool_{unary,stream}` | cancel with bash in flight (the CLI's timeout reason) | run failed; `late.txt` not written when the run ends | `cancel_requested` before `ended:cancelled`; the tool's dispatch `landed` after the run ended (the handler answers once told to stop; the bus never drops it, so no `cancelled@Collect`) | recorded |
//! | `cancel_at_hold_stream` | cancel while a call waits for approval | run failed; file untouched | `held`, `approval:held`, `cancel_requested`, `ended:cancelled`, then `cancelled { despawned_before_dispatch }` for the held call; never `approved`, no `released` after the last hold | recorded |
//! | `despawn_at_hold_stream` | the held call is despawned by the host | the runtime ends the run cancelled on its own; file untouched | `held` … `cancelled { despawned_before_dispatch }` with no `released` between (the `Despawning` set works), `ended:cancelled` last | recorded |

use crate::support::*;
use rig::observe::{Action, Stage};
use rig_ecs::bus::{Held, PendingEffect};

const MATRIX: &str = "observe_interruptions";

/// The reason the CLI's watchdog uses, so the reason detail is the product's.
const TIMEOUT_REASON: &str = "rigcoder --timeout-secs elapsed";

fn positions(facts: &[String], fact: &str) -> Vec<usize> {
    facts
        .iter()
        .enumerate()
        .filter(|(_, f)| f.as_str() == fact)
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn cancel_before_dispatch() {
    run(
        MATRIX,
        "cancel_before_dispatch",
        Config {
            source: Source::None,
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: never");
            cell.cancel("operator stop before dispatch");
            cell.drive();
            assert!(cell.ending().contains("operator stop"), "{}", cell.ending());
            assert!(cell.log().records.is_empty());
            let facts = cell.facts();
            // The run's first turn never assembled: no intent existed to
            // be despawned, so the request and the ending are the whole story.
            assert_eq!(facts, ["cancel_requested", "ended:cancelled"], "{facts:?}");
            let requested = cell
                .find(|a| matches!(a, Action::CancelRequested { .. }))
                .unwrap();
            assert_eq!(requested.stage, Stage::Runtime);
            assert_eq!(requested.subject.scope.as_deref(), Some("rigcoder/run/1"));
        },
    );
}

#[test]
fn cancel_mid_stream() {
    // The source: a long streamed answer, recorded whole.
    let long = || Config {
        max_tokens: 1500,
        additional_params: Some(serde_json::json!({
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}
        })),
        ..Config::streamed()
    };
    run(MATRIX, "long_answer_stream", long(), |cell| {
        cell.submit("Write a paragraph of at least two hundred words about rivers, then another about mountains.");
        cell.drive();
        assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
        let text: usize = cell
            .events()
            .iter()
            .filter_map(|e| match e {
                rigcoder::Event::Assistant { text } => Some(text.len()),
                _ => None,
            })
            .sum();
        eprintln!(
            "[{MATRIX}/long_answer_stream] answer {} chars, assistant events {text} chars",
            cell.answer().len()
        );
        assert!(text > 400, "{:?}", cell.events());
        assert_eq!(cell.facts(), ["issued", "landed", "ended:settled"]);
    });
    // The cell: the same frames, one every 150 ms, cancelled once the first
    // delta landed. Timing that a live or an instant replay cannot promise.
    run(
        MATRIX,
        "cancel_mid_stream",
        Config {
            source: paced(
                (MATRIX, "long_answer_stream"),
                0,
                std::time::Duration::from_millis(150),
            ),
            // The packet has a timing in it: how many deltas landed before
            // the cancel.
            volatile: true,
            ..long()
        },
        |cell| {
            cell.submit("Write a paragraph of at least two hundred words about rivers, then another about mountains.");
            cell.drive_until("the first delta landed", |world| {
                world
                    .resource::<rigcoder::Transcript>()
                    .events
                    .iter()
                    .any(|e| matches!(e, rigcoder::Event::Assistant { text } if !text.is_empty()))
            });
            cell.cancel("operator stop mid-stream");
            cell.drive();
            assert!(cell.ending().contains("mid-stream"), "{}", cell.ending());
            let at_end = cell.facts();
            eprintln!("[{MATRIX}/cancel_mid_stream] facts at the end: {at_end:?}");
            assert_eq!(
                at_end,
                ["issued", "cancel_requested", "ended:cancelled"],
                "{at_end:?}"
            );
            // The run ended with its completion still streaming: the product
            // keeps the run entity, so the bus never drops the dispatch —
            // it lands later, whole, and nothing reads it.
            cell.drive_more(std::time::Duration::from_secs(12), |world| {
                rigcoder::observations(world).is_some_and(|t| {
                    t.observations.iter().any(|o| {
                        matches!(o.action, Action::Landed { .. } | Action::Cancelled { .. })
                    })
                })
            });
            let facts = cell.facts();
            eprintln!("[{MATRIX}/cancel_mid_stream] facts after: {facts:?}");
            let after = cell
                .trace()
                .observations
                .into_iter()
                .find(|o| matches!(o.action, Action::Landed { .. }))
                .expect("the streaming dispatch lands after the run ended");
            assert!(
                after
                    .subject
                    .key
                    .as_ref()
                    .is_some_and(|k| k.as_str() == rigcoder::model::MODEL_KEY),
                "{after:?}"
            );
            assert!(after.seq as usize > positions(&facts, "ended:cancelled")[0]);
            eprintln!(
                "[{MATRIX}/cancel_mid_stream] the completion after the cancel: {:?}",
                after.action
            );
            assert_eq!(
                cell.count("cancelled@Collect"),
                0,
                "the bus never dropped it: {facts:?}"
            );
            // The record: one completion, whole, that no history consumed.
            let log = cell.log();
            assert_eq!(log.records.len(), 1, "{log:?}");
            assert!(
                log.records[0].outcome.is_ok(),
                "{:?}",
                log.records[0].outcome
            );
        },
    );
}

#[test]
fn cancel_with_bash_in_flight() {
    for stream in [false, true] {
        let name = format!(
            "cancel_mid_tool_{}",
            if stream { "stream" } else { "unary" }
        );
        run(MATRIX, &name, Config::delivery(stream), |cell| {
            cell.submit("Run this command: sleep 4; printf late > late.txt");
            cell.drive_until("bash in flight", |world| {
                world
                    .resource::<rigcoder::Transcript>()
                    .events
                    .iter()
                    .any(|e| matches!(e, rigcoder::Event::ToolCall { .. }))
            });
            cell.app.update();
            cell.cancel(TIMEOUT_REASON);
            cell.drive();
            assert!(cell.ending().contains(TIMEOUT_REASON), "{}", cell.ending());
            assert!(
                !cell.dir.join("late.txt").exists(),
                "the run ended before the tool"
            );
            let at_end = cell.facts();
            eprintln!("[{MATRIX}/{}] facts at the end: {at_end:?}", cell.name);
            let requested = positions(&at_end, "cancel_requested")[0];
            let ended = positions(&at_end, "ended:cancelled")[0];
            assert!(requested < ended, "{at_end:?}");
            // The run's ending does not wait for the tool: the bash handler
            // sees the cancellation, kills its process, and its dispatch
            // lands afterwards. Keep the world turning to observe it.
            cell.drive_more(std::time::Duration::from_secs(8), |world| {
                rigcoder::observations(world).is_some_and(|t| {
                    t.observations
                        .iter()
                        .filter(|o| matches!(o.action, Action::Landed { .. }))
                        .count()
                        >= 2
                })
            });
            let facts = cell.facts();
            eprintln!("[{MATRIX}/{}] facts after: {facts:?}", cell.name);
            let ended = positions(&facts, "ended:cancelled")[0];
            let tool_landed = cell
                .trace()
                .observations
                .into_iter()
                .find(|o| {
                    matches!(o.action, Action::Landed { .. })
                        && o.subject
                            .key
                            .as_ref()
                            .is_some_and(|k| k.as_str() == "tool:bash")
                })
                .expect("the in-flight tool lands after the run ended");
            assert!(tool_landed.seq as usize > ended, "{facts:?}");
            let Action::Landed { outcome } = &tool_landed.action else {
                unreachable!()
            };
            eprintln!(
                "[{MATRIX}/{}] the tool's outcome after the cancel: {outcome:?}",
                cell.name
            );
            // No `cancelled` for the tool: the bus never dropped the dispatch,
            // the handler answered after being told to stop.
            assert_eq!(cell.count("cancelled@Collect"), 0, "{facts:?}");
            let Action::CancelRequested { reason } = cell
                .find(|a| matches!(a, Action::CancelRequested { .. }))
                .unwrap()
                .action
            else {
                unreachable!()
            };
            assert!(
                reason
                    .detail
                    .as_deref()
                    .unwrap_or("")
                    .contains(TIMEOUT_REASON)
                    || reason.code.contains(TIMEOUT_REASON),
                "{reason:?}"
            );
        });
    }
}

#[test]
fn cancel_at_an_approval_hold() {
    run(
        MATRIX,
        "cancel_at_hold_stream",
        Config {
            approval: rigcoder::approval::ApprovalMode::Ask,
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Run this command: printf held > held.txt");
            cell.drive_until("an approval is pending", |world| {
                !world
                    .resource::<rigcoder::approval::Approvals>()
                    .pending
                    .is_empty()
            });
            cell.cancel("operator stop at hold");
            cell.drive();
            assert!(cell.ending().contains("at hold"), "{}", cell.ending());
            assert!(!cell.dir.join("held.txt").exists());
            let facts = cell.facts();
            eprintln!("[{MATRIX}/cancel_at_hold_stream] facts: {facts:?}");
            assert!(facts.contains(&"held".to_owned()));
            assert!(facts.contains(&"rigcoder/approval:held".to_owned()));
            assert!(!facts.contains(&"rigcoder/approval:approved".to_owned()));
            let requested = positions(&facts, "cancel_requested")[0];
            let ended = positions(&facts, "ended:cancelled")[0];
            let despawned = positions(&facts, "cancelled@Dispatch")[0];
            assert!(requested < ended && ended < despawned, "{facts:?}");
            // The held call left the world with the run: never released.
            let last_held = positions(&facts, "held").last().copied().unwrap();
            assert!(
                !facts[last_held..despawned].contains(&"released".to_owned()),
                "{facts:?}"
            );
            let despawn = cell
                .find(|a| matches!(a, Action::Cancelled { .. }))
                .unwrap();
            let Action::Cancelled { reason } = despawn.action else {
                unreachable!()
            };
            assert_eq!(reason.code, "despawned_before_dispatch");
            // One completion only: the held tool never reached the wire.
            assert_eq!(cell.log().records.len(), 1);
        },
    );
}

#[test]
fn despawn_at_an_approval_hold() {
    run(
        MATRIX,
        "despawn_at_hold_stream",
        Config {
            approval: rigcoder::approval::ApprovalMode::Ask,
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Run this command: printf gone > gone.txt");
            cell.drive_until("an approval is pending", |world| {
                !world
                    .resource::<rigcoder::approval::Approvals>()
                    .pending
                    .is_empty()
            });
            let held: Vec<bevy_ecs::entity::Entity> = cell
                .app
                .world_mut()
                .query_filtered::<bevy_ecs::entity::Entity, (
                    bevy_ecs::prelude::With<Held>,
                    bevy_ecs::prelude::With<PendingEffect>,
                )>()
                .iter(cell.app.world())
                .collect();
            assert_eq!(held.len(), 1);
            cell.app.world_mut().despawn(held[0]);
            cell.drive();
            let facts = cell.facts();
            eprintln!("[{MATRIX}/despawn_at_hold_stream] facts: {facts:?}");
            // Better than the documented limitation: the despawn is not
            // read as a release (the `Despawning` set from review round 2).
            let last_held = positions(&facts, "held").last().copied().unwrap();
            let despawned = positions(&facts, "cancelled@Dispatch")[0];
            assert!(despawned > last_held, "{facts:?}");
            assert!(
                !facts[last_held..despawned].contains(&"released".to_owned()),
                "a despawn is not a release: {facts:?}"
            );
            // The runtime notices its batch lost a call and ends the run.
            assert!(cell.ending().contains("Cancelled"), "{}", cell.ending());
            assert_eq!(facts.last().map(String::as_str), Some("ended:cancelled"));
            assert!(!cell.dir.join("gone.txt").exists());
            assert!(!facts.contains(&"rigcoder/approval:approved".to_owned()));
        },
    );
}
