//! Matrix B — gates and rewrites (`observe_gates`).
//!
//! | cell | dimension pinned | oracle | facts asserted | status |
//! |---|---|---|---|---|
//! | `ask_approve_{unary,stream}` | approval mode `ask`, approved by the host | file written | exactly `held`, `approval:held`, `approval:approved`, `released` before the tool's `issued` (one hold for the whole decision — Rig #2479) | recorded |
//! | `ask_refuse_{unary,stream}` | approval asked, refused | file untouched; the log holds no tool exchange | one `held`, `approval:held` then `approval:denied`, `denied@Gate:denied`, no `released` (the gate answers before it lets go of its hold) | recorded |
//! | `steer_deny_{unary,stream}` | a steering deny rule on bash | `Denied` transcript line, nothing prepared; the second recorded request carries the reason text | `rigcoder/steer` (rule `deny`), `denied@Gate:denied` | recorded |
//! | `scope_deny_{unary,stream}` | a file write outside the allowed scope | file absent | `rigcoder/steer` (rule `scope`), `denied@Gate:denied` | recorded |
//! | `stale_approval_{unary,stream}` | the call's arguments change after preparation began | denial names the change; the model's honest second call is approved and runs | `approval:held`, `approval:denied` (reason), `denied@Gate:denied`, then `approval:held`, `approval:approved` | recorded |
//! | `patched_{unary,stream}` | one serving layer rewrites the request | the recorded request carries the patch | effect record and cassette request contain the final patch | recorded |
//! | `patched_twice_stream` | two layers patch in sequence | recorded request carries both | effect record and cassette request contain both patches | recorded |
//! | `discarded` | a layer denies before the wire | run failed with the layer's reason, log empty | `issued`, `denied@Handler:layer_discarded` (emitter = the layer), `ended:provider`; no `landed` | no-wire |
//! | `replaced_{unary,stream}` | a layer replaces the answer after the record | run failed with the replacement; the record holds the handler's `Ok` | `landed` (Ok) then `replaced { recorded: Ok, consumed: Err }` at `Handler` (emitter = the layer), `ended:provider` | recorded |
//! | `shaped_stream` | an oversized `read_file` result cut for history | history cut, record whole | `rigcoder/result_shaped`; **no** library `replaced` (an in-place `Judge` rewrite raises no lifecycle event — documented gap) | recorded |
//! | `bounded_patch_stream` | a patch whose kind exceeds 64 KiB | recorded second request carries the 70 k-char history | effect record retains the large final request | recorded |

use crate::support::*;
use rig::observe::HostAction as _;
use rig::{
    effect::{EffectId, EffectKind, Outcome},
    error::{ErrorKind, ErrorReport},
    observe::{Action, OutcomeSummary, Stage},
    serve::{Decision, Intercept, Verdict},
};
use rig_ecs::bus::{Held, PendingEffect};

const MATRIX: &str = "observe_gates";

fn wait_for_hold(cell: &mut Cell) {
    cell.drive_until("an approval is pending", |world| {
        !world
            .resource::<rigcoder::approval::Approvals>()
            .pending
            .is_empty()
    });
}

fn approvals(cell: &Cell) -> Vec<String> {
    for fact in cell
        .trace()
        .observations
        .iter()
        .filter(|fact| matches!(fact.action, Action::Held { .. } | Action::Released))
    {
        assert_eq!(fact.emitter.name, "rigcoder/approval");
        assert!(fact.subject.order.is_some());
        assert!(fact.subject.scope.is_some());
    }
    cell.facts()
        .into_iter()
        .filter(|f| f.starts_with("rigcoder/approval"))
        .collect()
}

#[test]
fn ask_and_approve() {
    pair(
        MATRIX,
        "ask_approve",
        |stream| Config {
            approval: rigcoder::approval::ApprovalMode::Ask,
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Run this command: printf yes > ask.txt");
            wait_for_hold(cell);
            cell.app
                .world_mut()
                .resource_mut::<rigcoder::approval::Approvals>()
                .approve_next();
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert_eq!(
                std::fs::read_to_string(cell.dir.join("ask.txt")).unwrap(),
                "yes"
            );
            assert_eq!(
                approvals(cell),
                ["rigcoder/approval:held", "rigcoder/approval:approved"]
            );
            // The hold is the decision: held once while the reviewer decides,
            // released by the gate on approval, then dispatched.
            let facts = cell.facts();
            let tool_issued = cell
                .trace()
                .observations
                .iter()
                .position(|o| {
                    matches!(o.action, Action::Issued)
                        && o.subject
                            .key
                            .as_ref()
                            .is_some_and(|k| k.as_str() == "tool:bash")
                })
                .unwrap();
            let decision: Vec<&str> = facts[..tool_issued]
                .iter()
                .filter(|f| *f == "held" || *f == "released" || f.starts_with("rigcoder/approval"))
                .map(String::as_str)
                .collect();
            assert_eq!(
                decision,
                [
                    "held",
                    "rigcoder/approval:held",
                    "rigcoder/approval:approved",
                    "released"
                ],
                "{facts:?}"
            );
            let approval = cell
                .find(|a| matches!(a, Action::Host { payload, .. } if payload["decision"] == "approved"))
                .unwrap();
            let fact = rigcoder::observe::Approval::from_action(&approval.action)
                .unwrap()
                .unwrap();
            assert_eq!(fact.mode, "ask");
            assert_eq!(approval.stage, Stage::Host);
        },
    );
}

#[test]
fn ask_and_refuse() {
    pair(
        MATRIX,
        "ask_refuse",
        |stream| Config {
            approval: rigcoder::approval::ApprovalMode::Ask,
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Run this command: printf no > refused.txt");
            wait_for_hold(cell);
            cell.app
                .world_mut()
                .resource_mut::<rigcoder::approval::Approvals>()
                .deny_next();
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert!(!cell.dir.join("refused.txt").exists());
            assert_eq!(
                approvals(cell),
                ["rigcoder/approval:held", "rigcoder/approval:denied"]
            );
            assert_eq!(cell.count("denied@Gate:denied"), 1, "{:?}", cell.facts());
            // The denial removes the hold: no `released` anywhere — a hold
            // removed from an answered intent is the denial.
            let facts = cell.facts();
            assert_eq!(cell.count("released"), 0, "{facts:?}");
            assert_eq!(cell.count("held"), 1, "{facts:?}");
            // No exchange for the denied tool: two completions only.
            let log = cell.log();
            assert_eq!(log.records.len(), 2, "{log:?}");
            assert!(
                log.records
                    .iter()
                    .all(|r| r.key.as_str() == rigcoder::model::MODEL_KEY)
            );
            let denied = cell.find(|a| matches!(a, Action::Denied { .. })).unwrap();
            let Action::Denied { reason } = denied.action else {
                unreachable!()
            };
            assert_eq!(reason.code, ErrorKind::Denied.code());
            assert!(
                denied
                    .subject
                    .key
                    .as_ref()
                    .is_some_and(|k| k.as_str() == "tool:bash")
            );
        },
    );
}

#[test]
fn steer_deny_rule() {
    pair(
        MATRIX,
        "steer_deny",
        |stream| Config {
            steer: Some(|steer| {
                steer.deny.push((
                    r"^echo forbidden".to_owned(),
                    "that word is forbidden here".to_owned(),
                ));
            }),
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Run this command: echo forbidden");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert!(
                cell.events()
                    .iter()
                    .any(|e| matches!(e, rigcoder::Event::Denied { name, .. } if name == "bash"))
            );
            let facts = cell.facts();
            assert!(facts.contains(&"rigcoder/steer".to_owned()), "{facts:?}");
            assert!(
                facts.contains(&"denied@Gate:denied".to_owned()),
                "{facts:?}"
            );
            assert!(
                !facts.contains(&"rigcoder/approval:prepared".to_owned()),
                "{facts:?}"
            );
            let steer = cell
                .find(|a| matches!(a, Action::Host { kind, .. } if kind == "rigcoder/steer"))
                .unwrap();
            let fact = rigcoder::observe::SteerDenial::from_action(&steer.action)
                .unwrap()
                .unwrap();
            assert_eq!(fact.rule, "deny");
            assert_eq!(fact.reason, "that word is forbidden here");
            // The model read the reason: the second request carries it.
            let log = cell.log();
            assert_eq!(log.records.len(), 2);
            let rig::effect::EffectKind::Completion { request, .. } = &log.records[1].kind else {
                panic!()
            };
            assert!(
                serde_json::to_string(request)
                    .unwrap()
                    .contains("that word is forbidden here")
            );
            if !cell.recording() {
                let bodies = rig_cassette::recorded_interaction_bodies(
                    &cassette_root(),
                    PROVIDER,
                    &format!("{MATRIX}/{}", cell.name),
                );
                assert!(
                    bodies[1].0.contains("that word is forbidden here"),
                    "the wire carries the reason"
                );
            }
        },
    );
}

#[test]
fn scope_deny() {
    pair(
        MATRIX,
        "scope_deny",
        |stream| Config {
            scope: Some(|dir| rigcoder::steer::Scope {
                root: dir.to_owned(),
                allow: vec![dir.join("allowed")],
                deny: Vec::new(),
            }),
            prompt: "You are a test agent. When asked to write a file, call write_file with the given path and content, then reply with one short sentence.",
            ..Config::delivery(stream)
        },
        |cell, _| {
            std::fs::create_dir_all(cell.dir.join("allowed")).unwrap();
            cell.submit("Write the text hello into the file outside.txt");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert!(!cell.dir.join("outside.txt").exists());
            let steer = cell
                .find(|a| matches!(a, Action::Host { kind, .. } if kind == "rigcoder/steer"))
                .expect("a scope denial");
            let fact = rigcoder::observe::SteerDenial::from_action(&steer.action)
                .unwrap()
                .unwrap();
            assert_eq!(fact.rule, "scope");
            assert_eq!(fact.tool, "write_file");
            assert_eq!(cell.count("denied@Gate:denied"), 1, "{:?}", cell.facts());
        },
    );
}

#[test]
fn stale_approval() {
    pair(
        MATRIX,
        "stale_approval",
        |stream| Config {
            approval: rigcoder::approval::ApprovalMode::Ask,
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Run this command: printf stale > stale.txt");
            wait_for_hold(cell);
            // The bytes the reviewer saw are not the bytes that would run.
            let mut query = cell
                .app
                .world_mut()
                .query_filtered::<&mut PendingEffect, With<Held>>();
            let mut changed = 0;
            for mut pending in query.iter_mut(cell.app.world_mut()) {
                if let EffectKind::ToolCall { args, .. } = &mut pending.kind {
                    *args = args.replace("stale", "swapped");
                    changed += 1;
                }
            }
            assert_eq!(changed, 1);
            cell.app
                .world_mut()
                .resource_mut::<rigcoder::approval::Approvals>()
                .approve_next();
            // The model reads the refusal and asks again; that call is the
            // honest one, and the reviewer approves it.
            cell.drive_until("the second hold", |world| {
                let pending = &world.resource::<rigcoder::approval::Approvals>().pending;
                pending.len() == 1
                    && world
                        .resource::<rigcoder::Transcript>()
                        .events
                        .iter()
                        .any(|e| matches!(e, rigcoder::Event::Denied { .. }))
            });
            cell.app
                .world_mut()
                .resource_mut::<rigcoder::approval::Approvals>()
                .approve_next();
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert_eq!(
                std::fs::read_to_string(cell.dir.join("stale.txt")).unwrap(),
                "stale"
            );
            assert!(!cell.dir.join("swapped.txt").exists());
            assert_eq!(
                approvals(cell),
                [
                    "rigcoder/approval:held",
                    "rigcoder/approval:denied",
                    "rigcoder/approval:held",
                    "rigcoder/approval:approved"
                ],
                "{:?}",
                cell.facts()
            );
            let denied = cell
                .find(|a| matches!(a, Action::Host { payload, .. } if payload["decision"] == "denied"))
                .unwrap();
            let fact = rigcoder::observe::Approval::from_action(&denied.action)
                .unwrap()
                .unwrap();
            assert!(
                fact.reason
                    .as_deref()
                    .unwrap_or("")
                    .contains("changed after preparation"),
                "{fact:?}"
            );
            assert_eq!(cell.count("denied@Gate:denied"), 1);
            // Three completions: the call, the refusal read back, the answer.
            assert_eq!(
                cell.log()
                    .records
                    .iter()
                    .filter(|r| r.key.as_str() == rigcoder::model::MODEL_KEY)
                    .count(),
                3
            );
        },
    );
}

use bevy_ecs::prelude::With;

/// A layer that pins the temperature.
struct Cooler(f64);

impl Intercept for Cooler {
    fn name(&self) -> String {
        format!("cooler-{}", self.0)
    }

    async fn before(&self, _: EffectId, kind: &EffectKind) -> Decision {
        let EffectKind::Completion { request, stream } = kind else {
            return Decision::Proceed;
        };
        let mut request = request.clone();
        request.temperature = Some(self.0);
        Decision::Patch(EffectKind::Completion {
            request,
            stream: *stream,
        })
    }

    async fn after(
        &self,
        _: EffectId,
        _: &EffectKind,
        _: &Result<Outcome, ErrorReport>,
    ) -> Verdict {
        Verdict::Keep
    }
}

/// A layer that caps the output.
struct Capper(u64);

impl Intercept for Capper {
    fn name(&self) -> String {
        "capper".into()
    }

    async fn before(&self, _: EffectId, kind: &EffectKind) -> Decision {
        let EffectKind::Completion { request, stream } = kind else {
            return Decision::Proceed;
        };
        let mut request = request.clone();
        request.max_tokens = Some(self.0);
        Decision::Patch(EffectKind::Completion {
            request,
            stream: *stream,
        })
    }

    async fn after(
        &self,
        _: EffectId,
        _: &EffectKind,
        _: &Result<Outcome, ErrorReport>,
    ) -> Verdict {
        Verdict::Keep
    }
}

/// A layer that never lets a request through.
struct Bouncer;

impl Intercept for Bouncer {
    fn name(&self) -> String {
        "bouncer".into()
    }

    async fn before(&self, _: EffectId, _: &EffectKind) -> Decision {
        Decision::deny("not on the list")
    }

    async fn after(
        &self,
        _: EffectId,
        _: &EffectKind,
        _: &Result<Outcome, ErrorReport>,
    ) -> Verdict {
        Verdict::Keep
    }
}

/// A layer that replaces every answer with a provider report.
struct Replacer;

impl Intercept for Replacer {
    fn name(&self) -> String {
        "replacer".into()
    }

    async fn before(&self, _: EffectId, _: &EffectKind) -> Decision {
        Decision::Proceed
    }

    async fn after(
        &self,
        _: EffectId,
        _: &EffectKind,
        _: &Result<Outcome, ErrorReport>,
    ) -> Verdict {
        Verdict::Replace(Err(ErrorReport::new(
            ErrorKind::Provider,
            "the answer was withdrawn by policy",
        )
        .with_retryable(false)))
    }
}

#[test]
fn a_layer_patch() {
    pair(
        MATRIX,
        "patched",
        |stream| Config {
            layers: Some(|h| h.layered(Cooler(0.1))),
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Reply with the single word: patched");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            let facts = cell.lifecycle_facts();
            assert!(cell.count("adapter") > 0);
            assert_eq!(facts, ["issued", "landed", "ended:settled"], "{facts:?}");
            // The record holds what was served.
            let record = &cell.log().records[0];
            let EffectKind::Completion { request, .. } = &record.kind else {
                panic!()
            };
            assert_eq!(request.temperature, Some(0.1));
            if !cell.recording() {
                let wire = rig_cassette::recorded_json_request(
                    &cassette_root(),
                    PROVIDER,
                    &format!("{MATRIX}/{}", cell.name),
                );
                assert_eq!(wire["generationConfig"]["temperature"], 0.1, "{wire}");
            }
        },
    );
}

#[test]
fn two_layers_patch_in_order() {
    run(
        MATRIX,
        "patched_twice_stream",
        Config {
            layers: Some(|h| h.layered(Capper(64)).layered(Cooler(0.2))),
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: twice");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            let facts = cell.lifecycle_facts();
            assert!(cell.count("adapter") > 0);
            assert_eq!(facts, ["issued", "landed", "ended:settled"], "{facts:?}");
            let record = &cell.log().records[0];
            let EffectKind::Completion { request, .. } = &record.kind else {
                panic!()
            };
            assert_eq!(
                (request.temperature, request.max_tokens),
                (Some(0.2), Some(64))
            );
        },
    );
}

#[test]
fn a_layer_discard_never_reaches_the_wire() {
    run(
        MATRIX,
        "discarded",
        Config {
            layers: Some(|h| h.layered(Bouncer)),
            source: Source::None,
            ..Config::streamed()
        },
        |cell| {
            cell.submit("Reply with the single word: never");
            cell.drive();
            assert!(
                cell.failure().kind == "denied"
                    && cell.failure().message.contains("not on the list"),
                "{}",
                cell.ending()
            );
            assert!(
                cell.log().records.is_empty(),
                "no exchange: {:?}",
                cell.log()
            );
            let facts = cell.facts();
            assert_eq!(
                facts,
                [
                    "issued",
                    "denied@Handler:layer_discarded",
                    "ended:provider",
                    "rigcoder/failure"
                ],
                "a discarded dispatch never lands: {facts:?}"
            );
            let denied = cell.find(|a| matches!(a, Action::Denied { .. })).unwrap();
            assert_eq!(
                cell.failure().boundary,
                rigcoder::failure::FailureBoundary::Host
            );
            assert!(cell.failure().adapter.is_none());
            assert_eq!(denied.emitter.name, "bouncer", "the layer names itself");
            assert!(
                denied.subject.effect.is_some(),
                "issued, then discarded: {denied:?}"
            );
        },
    );
}

#[test]
fn a_judge_replacement() {
    pair(
        MATRIX,
        "replaced",
        |stream| Config {
            layers: Some(|h| h.layered(Replacer)),
            ..Config::delivery(stream)
        },
        |cell, _| {
            cell.submit("Reply with the single word: kept");
            cell.drive();
            assert!(
                cell.failure().kind == "provider"
                    && cell.failure().message.contains("withdrawn by policy"),
                "{}",
                cell.ending()
            );
            // The record holds the handler's answer.
            let record = &cell.log().records[0];
            assert!(record.outcome.is_ok(), "{record:?}");
            let facts = cell.lifecycle_facts();
            assert!(cell.count("adapter") > 0);
            assert_eq!(
                cell.failure().boundary,
                rigcoder::failure::FailureBoundary::Host
            );
            assert!(
                cell.failure().adapter.is_none(),
                "host replacement is not a failed provider attempt"
            );
            assert_eq!(
                facts,
                [
                    "issued",
                    "landed",
                    "replaced",
                    "ended:provider",
                    "rigcoder/failure"
                ],
                "{facts:?}"
            );
            let replaced = cell.find(|a| matches!(a, Action::Replaced { .. })).unwrap();
            assert_eq!(replaced.stage, Stage::Handler);
            assert_eq!(replaced.emitter.name, "replacer", "the layer names itself");
            let Action::Replaced { recorded, consumed } = replaced.action else {
                unreachable!()
            };
            assert!(
                matches!(recorded, OutcomeSummary::Ok { .. }),
                "{recorded:?}"
            );
            assert!(
                matches!(consumed, OutcomeSummary::Err { ref reason, retryable: false } if reason.code == "provider"),
                "{consumed:?}"
            );
        },
    );
}

#[test]
fn an_oversized_result_is_shaped_in_place() {
    run(
        MATRIX,
        "shaped_stream",
        Config {
            prompt: "You are a test agent. When asked to read a file, call read_file with that path. After the result arrives, reply with exactly: done",
            ..Config::streamed()
        },
        |cell| {
            std::fs::write(cell.dir.join("big.txt"), "b".repeat(40_000)).unwrap();
            cell.submit("Read the file big.txt");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            assert_eq!(
                cell.count("rigcoder/result_shaped"),
                1,
                "{:?}",
                cell.facts()
            );
            assert_eq!(
                cell.count("replaced"),
                0,
                "an in-place Judge rewrite is the host's own fact only"
            );
            let shaped = cell
                .find(
                    |a| matches!(a, Action::Host { kind, .. } if kind == "rigcoder/result_shaped"),
                )
                .unwrap();
            let fact = rigcoder::observe::ResultShaped::from_action(&shaped.action)
                .unwrap()
                .unwrap();
            assert_eq!((fact.tool.as_str(), fact.kept), ("read_file", 30_000));
            assert!(fact.chars > 40_000, "line-numbered output: {fact:?}");
            assert!(
                shaped
                    .subject
                    .key
                    .as_ref()
                    .is_some_and(|k| k.as_str() == "tool:read_file"),
                "{shaped:?}"
            );
            // The record keeps the whole answer; the next request carries the cut.
            let log = cell.log();
            let tool = log
                .records
                .iter()
                .find(|r| r.key.as_str() == "tool:read_file")
                .unwrap();
            assert!(serde_json::to_string(&tool.outcome).unwrap().len() > 40_000);
        },
    );
}

#[test]
fn large_patched_requests_remain_in_the_effect_log() {
    run(
        MATRIX,
        "bounded_patch_stream",
        Config {
            layers: Some(|h| h.layered(Cooler(0.3))),
            steer: Some(|steer| steer.max_result_chars = 200_000),
            prompt: "You are a test agent. When asked to read a file, call read_file with that path. After the result arrives, reply with exactly: done",
            ..Config::streamed()
        },
        |cell| {
            let lines: String = (1..=700)
                .map(|i| format!("{i:04} {}\n", "h".repeat(95)))
                .collect();
            std::fs::write(cell.dir.join("huge.txt"), lines).unwrap();
            cell.submit("Read the file huge.txt with read_file, then reply with exactly the word: done. Do nothing else.");
            cell.drive();
            assert_eq!(cell.ending(), "settled", "{:?}", cell.events());
            // The exchange record is whole.
            let record = cell
                .log()
                .records
                .iter()
                .rev()
                .find(|r| r.key.as_str() == rigcoder::model::MODEL_KEY)
                .cloned()
                .unwrap();
            assert!(serde_json::to_string(&record.kind).unwrap().len() > 70_000);
        },
    );
}
