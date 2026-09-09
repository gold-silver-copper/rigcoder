use super::*;

#[test]
fn job_preserves_missing_scores_and_existing_selection_precedence() {
    let root = std::env::temp_dir().join(format!(
        "rigcoder-score-evidence-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cases = [
        ("missing", None, None, None),
        ("malformed", Some("invalid"), Some("{broken"), None),
        ("no_reward", None, Some("{}"), None),
        ("zero", None, Some("{\"reward\":0}"), Some(0.0)),
        (
            "verifier_wins",
            Some("0"),
            Some("{\"reward\":1}"),
            Some(0.0),
        ),
        (
            "result_fallback",
            Some("invalid"),
            Some("{\"reward\":1}"),
            Some(1.0),
        ),
        (
            "nested",
            None,
            Some("{\"verifier_result\":{\"rewards\":{\"reward\":0.5}}}"),
            Some(0.5),
        ),
        ("nonfinite", Some("NaN"), None, None),
        ("infinite", Some("inf"), Some("{\"reward\":0}"), None),
    ];
    for (name, verifier, result, _) in &cases {
        let trial = root.join(name);
        std::fs::create_dir_all(trial.join("agent")).unwrap();
        std::fs::create_dir_all(trial.join("verifier")).unwrap();
        std::fs::write(
            trial.join("agent/transcript.jsonl"),
            "{\"kind\":\"settled\"}\n",
        )
        .unwrap();
        std::fs::write(trial.join("agent/observations.json"), include_str!(
            "../../../../fixtures/evidence/gemini/observe_wire/unary_http_boundary/observations.json"
        )).unwrap();
        if let Some(text) = verifier {
            std::fs::write(trial.join("verifier/reward.txt"), text).unwrap();
        }
        if let Some(text) = result {
            std::fs::write(trial.join("result.json"), text).unwrap();
        }
    }
    let digest = job(&root, &root).unwrap();
    for (name, _, _, expected) in cases {
        let trial = digest
            .trials
            .iter()
            .find(|trial| trial.trial == name)
            .unwrap();
        assert_eq!(trial.reward, expected, "{name}");
        assert_eq!(trial.observed.attempts.len(), 1);
        assert_eq!(trial.observed.attempts[0].status, Some(200));
        assert_eq!(
            serde_json::to_value(trial).unwrap()["reward"],
            serde_json::to_value(expected).unwrap()
        );
        if name == "nonfinite" {
            assert!(
                trial.selection_reward().is_nan(),
                "legacy routing is preserved"
            );
        } else if name == "infinite" {
            assert_eq!(trial.selection_reward(), f64::INFINITY);
        } else {
            assert_eq!(trial.selection_reward(), expected.unwrap_or(0.0));
        }
    }
    assert_eq!(digest.passed.trials, 2);
    assert_eq!(digest.failed.trials, 6);
    assert!(render(&digest).contains("unavailable evidence"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn job_links_recorded_identity_without_claiming_candidate_revision_or_changing_scores() {
    let root = std::env::temp_dir().join(format!(
        "rigcoder-digest-metadata-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let job_dir = root.join("jobs/evaluation");
    let trial = job_dir.join("directory_guess__2");
    std::fs::create_dir_all(trial.join("agent")).unwrap();
    std::fs::create_dir_all(trial.join("verifier")).unwrap();
    std::fs::create_dir_all(root.join("harness")).unwrap();
    std::fs::write(
        trial.join("agent/transcript.jsonl"),
        "{\"kind\":\"settled\"}\n",
    )
    .unwrap();
    std::fs::write(
        trial.join("agent/observations.json"),
        include_str!(
            "../../../../fixtures/evidence/gemini/observe_wire/unary_http_boundary/observations.json"
        ),
    )
    .unwrap();
    std::fs::write(trial.join("verifier/reward.txt"), "0").unwrap();
    std::fs::write(
        trial.join("result.json"),
        serde_json::json!({"task": "actual__task", "attempt": 7, "reward": 1}).to_string(),
    )
    .unwrap();
    let entry = crate::ledger::Entry {
        generation: Some(3),
        slice: "dev".into(),
        commit: "recorded-base-before-rejected-edits".into(),
        decision: Some(crate::stats::Decision::Reverted),
        best_score: 1.0,
        best_ci_low: 0.5,
        model: "recorded-model".into(),
        attempts: 8,
        lane: Some(crate::evolve::Lane::Prompt),
        meta_commit: Some("editor-revision".into()),
        job_dir: "jobs/evaluation".into(),
        time: 1,
        summary: Default::default(),
    };
    let ledger = root.join("harness/ledger.jsonl");
    let row = serde_json::to_string(&entry).unwrap();
    std::fs::write(&ledger, &row).unwrap();
    let (digest, output) = write(&job_dir, &root).unwrap();
    let trial_facts = &digest.trials[0];
    assert_eq!(trial_facts.trial, "directory_guess__2");
    assert_eq!(trial_facts.task.as_deref(), Some("actual__task"));
    assert_eq!(trial_facts.task_source, Some(TaskSource::ResultFile));
    assert_eq!(trial_facts.attempt, Some(7));
    assert_eq!(
        trial_facts.reward,
        Some(0.0),
        "verifier reward still takes precedence"
    );
    assert_eq!(trial_facts.ending, "settled");
    assert!(trial_facts.observed.complete);
    assert_eq!(digest.failed.trials, 1);
    assert_eq!(digest.passed.trials, 0);
    let evaluation = digest.evaluation.as_ref().unwrap();
    assert_eq!(evaluation.recorded_commit, entry.commit);
    assert_eq!(evaluation.meta_commit, entry.meta_commit);
    assert_eq!(evaluation.generation, Some(3));
    assert_eq!(evaluation.slice, "dev");
    assert_eq!(evaluation.model, "recorded-model");
    assert_eq!(evaluation.attempts, 8);
    assert_eq!(evaluation.lane, Some(crate::evolve::Lane::Prompt));
    assert!(evaluation.candidate_revision.is_none());
    assert!(evaluation.configuration_id.is_none());
    let serialized: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
    assert!(
        serialized["evaluation"]
            .get("candidate_revision")
            .unwrap()
            .is_null()
    );
    assert!(
        serialized["evaluation"]
            .get("configuration_id")
            .unwrap()
            .is_null()
    );

    // Current harness rows use absolute paths. Relative/absolute aliases of
    // the same existing directory still refer to one job, not two identities.
    let absolute = crate::ledger::Entry {
        job_dir: job_dir.canonicalize().unwrap().display().to_string(),
        ..entry.clone()
    };
    let absolute_row = serde_json::to_string(&absolute).unwrap();
    std::fs::write(&ledger, &absolute_row).unwrap();
    assert!(job(&job_dir, &root).unwrap().evaluation.is_some());
    let alias = crate::ledger::Entry {
        job_dir: "jobs/./evaluation".into(),
        ..entry.clone()
    };
    let alias_row = serde_json::to_string(&alias).unwrap();
    std::fs::write(&ledger, &alias_row).unwrap();
    assert!(job(&job_dir, &root).unwrap().evaluation.is_some());

    // Missing, malformed and ambiguous provenance cannot change trial facts.
    for text in [
        String::new(),
        "malformed".into(),
        format!("{row}\n{row}"),
        format!("{row}\nmalformed"),
        format!("{absolute_row}\n{alias_row}"),
    ] {
        std::fs::write(&ledger, text).unwrap();
        let unknown = job(&job_dir, &root).unwrap();
        assert!(unknown.evaluation.is_none());
        assert_eq!(unknown.trials, digest.trials);
    }
    // A different job with the same basename is not an identity match.
    std::fs::create_dir_all(root.join("other/evaluation")).unwrap();
    let other = crate::ledger::Entry {
        job_dir: "other/evaluation".into(),
        ..entry
    };
    std::fs::write(&ledger, serde_json::to_string(&other).unwrap()).unwrap();
    assert!(job(&job_dir, &root).unwrap().evaluation.is_none());
    std::fs::remove_file(&ledger).unwrap();
    assert!(job(&job_dir, &root).unwrap().evaluation.is_none());

    std::fs::write(trial.join("result.json"), r#"{"task":"partial__task"}"#).unwrap();
    let partial = job(&job_dir, &root).unwrap();
    assert_eq!(partial.trials[0].task.as_deref(), Some("partial__task"));
    assert_eq!(partial.trials[0].task_source, Some(TaskSource::ResultFile));
    assert_eq!(partial.trials[0].attempt, None);
    std::fs::write(trial.join("result.json"), r#"{"attempt":7}"#).unwrap();
    let partial = job(&job_dir, &root).unwrap();
    assert_eq!(partial.trials[0].task.as_deref(), Some("directory_guess"));
    assert_eq!(
        partial.trials[0].task_source,
        Some(TaskSource::DirectoryName)
    );
    assert_eq!(partial.trials[0].attempt, Some(7));

    // Without recorded identity, keep a labelled directory fallback and no
    // invented attempt ordinal. Names without a delimiter remain unknown.
    std::fs::write(trial.join("result.json"), "malformed").unwrap();
    let legacy = job(&job_dir, &root).unwrap();
    assert_eq!(legacy.trials[0].task.as_deref(), Some("directory_guess"));
    assert_eq!(
        legacy.trials[0].task_source,
        Some(TaskSource::DirectoryName)
    );
    assert_eq!(legacy.trials[0].attempt, None);
    std::fs::rename(&trial, job_dir.join("anonymous")).unwrap();
    let anonymous = job(&job_dir, &root).unwrap();
    assert_eq!(anonymous.trials[0].task, None);
    assert_eq!(anonymous.trials[0].task_source, None);
    assert_eq!(anonymous.trials[0].attempt, None);
    std::fs::remove_dir_all(root).unwrap();
}
