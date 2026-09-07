//! Exercise the actual CLI with isolated repositories and deterministic Docker,
//! Cargo, and agent substitutes. No daemon, model, or network is involved.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, json};

static NEXT: AtomicU64 = AtomicU64::new(0);
const PROMPT: &str = "crates/rigcoder/src/prompt.md";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "rigcoder-bench-regression-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fixture = Self { root };
        for file in [
            PROMPT,
            "crates/rigcoder/src/tools.rs",
            "crates/rigcoder/src/lib.rs",
            "crates/rigcoder/src/session.rs",
            "crates/rigcoder-cli/src/main.rs",
            "README.md",
        ] {
            fixture.write(file, "baseline\n");
        }
        fixture.write(
            ".gitignore",
            "harness/runs/\nharness/bin/\nharness/tasks/\nfakebin/\nmock.log\nmock-state\n",
        );
        fixture.write("harness/slices/dev.txt", "a\nb\n");
        fixture.write("harness/slices/holdout.txt", "heldout\n");
        fixture.write("harness/ledger.jsonl", "");
        fixture.script(
            "harness/build-linux.sh",
            "#!/bin/sh\nmkdir -p harness/bin\ncp crates/rigcoder/src/prompt.md harness/bin/rigcoder-linux-$ARCH\n",
        );
        for task in ["a", "b", "heldout"] {
            fixture.write(&format!("harness/tasks/{task}/task.toml"), "");
            fixture.write(&format!("harness/tasks/{task}/instruction.md"), "solve it");
            fixture.write(
                &format!("harness/tasks/{task}/environment/Dockerfile"),
                "FROM scratch\nWORKDIR /app\n",
            );
            fixture.write(&format!("harness/tasks/{task}/tests/test.sh"), "exit 0\n");
        }
        fixture.write(
            &format!("harness/bin/rigcoder-linux-{}", std::env::consts::ARCH),
            "baseline\n",
        );
        fixture.script(
            "fakebin/docker",
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$MOCK_LOG"
case "$1" in
version) echo 25 ;;
build)
    case "$*" in
    *rigcoder-bench/b:*) if [ "$MOCK_FAIL_BUILD" = 1 ]; then echo 'forced build failure' >&2; exit 1; fi ;;
    esac ;;
cp)
    case "$3" in
    *:/usr/local/bin/rigcoder) cat "$2" > "$MOCK_STATE"; printf 'upload:%s\n' "$(cat "$2")" >> "$MOCK_LOG" ;;
    esac ;;
exec)
    case "$*" in
    *'tar -xf /restore/workspace.tar'*) exit "${MOCK_RESTORE_EXIT:-0}" ;;
    *'cat /logs/verifier/reward.txt')
        if [ -n "$MOCK_REWARD" ]; then echo "$MOCK_REWARD"
        elif [ "$(cat "$MOCK_STATE")" = candidate ]; then echo 0
        else echo 1; fi ;;
    *'bash /tests/test.sh'*) exit "${MOCK_VERIFIER_EXIT:-0}" ;;
    esac ;;
esac
"#,
        );
        fixture.script("fakebin/timeout", "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'timeout (GNU coreutils)'; exit 0; fi\nshift\nshift\nexec \"$@\"\n");
        fixture.script("fakebin/cargo", "#!/bin/sh\nexit 0\n");
        fixture.script(
            "fakebin/agent",
            r#"#!/bin/sh
printf 'candidate\n' > crates/rigcoder/src/prompt.md
printf 'unapproved\n' > README.md
printf 'new file\n' > unexpected.txt
printf 'staged new file\n' > staged-new.txt
printf 'corrupt ledger\n' > harness/ledger.jsonl
git add crates/rigcoder/src/prompt.md README.md staged-new.txt harness/ledger.jsonl
"#,
        );
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.name", "Bench regression"]);
        fixture.git(&["config", "user.email", "bench@example.invalid"]);
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "baseline"]);
        fixture
    }

    fn write(&self, file: &str, content: &str) {
        let path = self.root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn script(&self, file: &str, content: &str) {
        self.write(file, content);
        fs::set_permissions(self.root.join(file), fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rigcoder-bench"))
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .args([
                "-m",
                "mock/model",
                "-k",
                "1",
                "-n",
                "1",
                "--job-name",
                "same-prefix",
            ])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("fakebin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("RIGCODER_HOST_BIN", self.root.join("fakebin/agent"))
            .env("MOCK_LOG", self.root.join("mock.log"))
            .env("MOCK_STATE", self.root.join("mock-state"))
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    fn ledger(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("harness/ledger.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn evaluation_only_preserves_worktree_index_branch_and_history() {
    for reward in ["0", "1"] {
        let f = Fixture::new();
        f.write(PROMPT, "user draft\n");
        f.write("README.md", "staged user draft\n");
        f.git(&["add", "README.md"]);
        let before = f.git(&["rev-parse", "HEAD"]);
        let index = f.git(&["diff", "--cached"]);
        success(&f.run(
            &[
                "iterate",
                "--no-improve",
                "--no-build",
                "--generations",
                "2",
                "--holdout",
                "false",
            ],
            &[("MOCK_REWARD", reward)],
        ));
        assert_eq!(f.git(&["rev-parse", "HEAD"]), before);
        assert_eq!(f.git(&["branch", "--show-current"]), "main");
        assert_eq!(f.git(&["diff", "--cached"]), index);
        assert_eq!(
            fs::read_to_string(f.root.join(PROMPT)).unwrap(),
            "user draft\n"
        );
        let ledger = f.ledger();
        assert_eq!(ledger.len(), 2);
        assert!(ledger.iter().all(|entry| entry["decision"].is_null()));
        assert_ne!(ledger[0]["job_dir"], ledger[1]["job_dir"]);
    }
}

#[test]
fn self_edit_rejects_dirty_input_existing_branch_and_stale_binary_mode() {
    for kind in ["worktree", "index", "untracked", "branch", "no-build"] {
        let f = Fixture::new();
        match kind {
            "worktree" => f.write(PROMPT, "user work"),
            "index" => {
                f.write("README.md", "user work");
                f.git(&["add", "README.md"]);
            }
            "untracked" => f.write("new-user-file", "user work"),
            "branch" => {
                f.git(&["branch", "evolve"]);
            }
            _ => {}
        }
        let before = f.git(&["rev-parse", "HEAD"]);
        let status = f.git(&["status", "--porcelain"]);
        let mut args = vec!["iterate", "--generations", "1", "--holdout", "false"];
        if kind == "no-build" {
            args.push("--no-build");
        }
        let output = f.run(&args, &[]);
        assert!(!output.status.success(), "unexpected success: {kind}");
        assert_eq!(f.git(&["rev-parse", "HEAD"]), before);
        assert_eq!(f.git(&["status", "--porcelain"]), status);
        assert!(!f.root.join("mock.log").exists());
    }
}

#[test]
fn incomplete_builds_and_invalid_verifiers_never_publish_scores() {
    for env in [
        [("MOCK_FAIL_BUILD", "1")],
        [("MOCK_REWARD", "NaN")],
        [("MOCK_VERIFIER_EXIT", "1")],
    ] {
        let f = Fixture::new();
        let output = f.run(&["run", "--no-build"], &env);
        assert!(!output.status.success());
        assert!(f.ledger().is_empty());
        let jobs: Vec<_> = fs::read_dir(f.root.join("harness/runs"))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].path().join("error.txt").is_file());
        assert!(!jobs[0].path().join("summary.json").exists());
    }
}

#[test]
fn rejected_candidate_restores_staged_changes_and_rebuilds_holdout() {
    let f = Fixture::new();
    let before = f.git(&["rev-parse", "HEAD"]);
    success(&f.run(&["iterate", "--generations", "2"], &[]));
    assert_eq!(f.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(
        fs::read_to_string(f.root.join(PROMPT)).unwrap(),
        "baseline\n"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("README.md")).unwrap(),
        "baseline\n"
    );
    assert!(!f.root.join("unexpected.txt").exists());
    assert!(!f.root.join("staged-new.txt").exists());
    assert!(f.git(&["diff", "--cached"]).is_empty());
    let ledger = f.ledger();
    assert_eq!(ledger.len(), 3);
    assert_eq!(ledger[0]["decision"], "kept");
    assert_eq!(ledger[1]["decision"], "reverted");
    assert_eq!(ledger[2]["slice"], "holdout");
    assert_eq!(ledger[2]["score"], 1.0);
    assert_eq!(
        ledger
            .iter()
            .map(|entry| entry["job_dir"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    let log = fs::read_to_string(f.root.join("mock.log")).unwrap();
    let uploads: Vec<_> = log
        .lines()
        .filter(|line| line.starts_with("upload:"))
        .collect();
    assert_eq!(
        uploads,
        [
            "upload:baseline",
            "upload:baseline",
            "upload:candidate",
            "upload:candidate",
            "upload:baseline"
        ]
    );
    let agent = log.find("--task-file").unwrap();
    let tests = log.find("harness/tasks/a/tests ").unwrap();
    assert!(agent < tests, "verifier was exposed before the agent ran");
}

#[test]
fn historical_scores_from_other_runs_do_not_replace_current_baseline() {
    let f = Fixture::new();
    let historical = json!({
        "generation": 0, "slice": "dev", "commit": "other", "decision": "kept", "best_score": -1.0,
        "best_ci_low": -1.0, "model": "other/model", "attempts": 999, "job_dir": "old", "time": 0,
        "score": 1.0, "pass1": 1.0, "passk": 1.0, "ci_low": 0.999, "ci_high": 1.0,
        "trials": 999, "tasks": 1, "rewards": {}, "cost": {"input_tokens":0,"output_tokens":0,"cache_tokens":0,"tool_calls":0,"wall_seconds":0.0}
    });
    f.write("harness/ledger.jsonl", &format!("{historical}\n"));
    success(&f.run(
        &["iterate", "--generations", "1", "--holdout", "false"],
        &[("MOCK_REWARD", "0")],
    ));
    assert_eq!(f.ledger()[1]["decision"], "kept");
    assert_eq!(f.ledger()[1]["best_score"], -1.0);
}

#[test]
fn canceled_worktree_changes_cannot_smuggle_a_forbidden_staged_edit() {
    let f = Fixture::new();
    f.script(
        "fakebin/agent",
        r#"#!/bin/sh
printf 'candidate\n' > crates/rigcoder/src/prompt.md
printf 'forbidden staged content\n' > README.md
git add README.md
git show HEAD:README.md > README.md
"#,
    );
    success(&f.run(
        &["iterate", "--generations", "2", "--holdout", "false"],
        &[("MOCK_REWARD", "1")],
    ));
    assert_eq!(f.git(&["show", "HEAD:README.md"]), "baseline");
    assert_eq!(
        f.git(&["show", "HEAD:crates/rigcoder/src/prompt.md"]),
        "candidate"
    );
    assert!(f.git(&["diff", "--cached"]).is_empty());
    assert_eq!(
        fs::read_to_string(f.root.join("README.md")).unwrap(),
        "baseline\n"
    );
}

#[test]
fn a_meta_agent_switching_branch_at_the_same_commit_stops_the_loop() {
    let f = Fixture::new();
    let before = f.git(&["rev-parse", "HEAD"]);
    f.script(
        "fakebin/agent",
        "#!/bin/sh\nprintf 'candidate\\n' > crates/rigcoder/src/prompt.md\ngit checkout -q main\n",
    );
    let output = f.run(
        &["iterate", "--generations", "2", "--holdout", "false"],
        &[],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("changed HEAD or branch"));
    assert_eq!(f.git(&["rev-parse", "main"]), before);
    assert_eq!(f.git(&["rev-parse", "evolve"]), before);
    assert_eq!(f.ledger().len(), 1);
}

#[test]
fn a_missing_host_timeout_fails_before_starting_docker() {
    let f = Fixture::new();
    fs::remove_file(f.root.join("fakebin/timeout")).unwrap();
    let output = f.run(
        &["run", "--no-build"],
        &[("PATH", f.root.join("fakebin").to_str().unwrap())],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("GNU timeout is required"));
    assert!(!f.root.join("mock.log").exists());
    assert!(f.ledger().is_empty());
}

#[test]
fn rejected_notes_are_archived_and_historical_notes_survive() {
    let f = Fixture::new();
    f.write("harness/notes/historical.md", "kept history\n");
    f.git(&["add", "harness/notes/historical.md"]);
    f.git(&["commit", "-qm", "historical note"]);
    f.script(
        "fakebin/agent",
        r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
    if [ "$1" = --allow ]; then
        case "$2" in */harness/notes/*) note=$2 ;; esac
        shift
    fi
    shift
done
printf 'candidate\n' > crates/rigcoder/src/prompt.md
printf 'candidate rationale\n' > "$note"
"#,
    );
    success(&f.run(
        &["iterate", "--generations", "2", "--holdout", "false"],
        &[],
    ));
    assert_eq!(
        fs::read_to_string(f.root.join("harness/notes/historical.md")).unwrap(),
        "kept history\n"
    );
    let ledger = f.ledger();
    let rejected = PathBuf::from(ledger[1]["job_dir"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(rejected.join("improvement.md")).unwrap(),
        "candidate rationale\n"
    );
    assert_eq!(
        fs::read_dir(f.root.join("harness/notes")).unwrap().count(),
        1
    );
    assert!(f.git(&["diff", "--cached"]).is_empty());
}

#[test]
fn tools_lane_commits_new_files_without_optional_paths() {
    let f = Fixture::new();
    f.script("fakebin/agent", "#!/bin/sh\nmkdir -p crates/rigcoder/src/tools\nprintf 'new tool\\n' > crates/rigcoder/src/tools/new.rs\n");
    success(&f.run(
        &[
            "iterate",
            "--generations",
            "2",
            "--holdout",
            "false",
            "--lane",
            "tools",
        ],
        &[],
    ));
    assert_eq!(
        f.git(&["show", "HEAD:crates/rigcoder/src/tools/new.rs"]),
        "new tool"
    );
    assert!(!f.root.join("crates/rigcoder/src/steer.rs").exists());
    assert!(f.git(&["diff", "--cached"]).is_empty());
}

#[test]
fn branching_checks_restore_failure_and_installs_tests_after_resume() {
    for restore_exit in ["0", "7"] {
        let f = Fixture::new();
        let trial = "harness/runs/source/a__1";
        f.write(&format!("{trial}/agent/scenes/turn-001.scene.json"), "{}");
        f.write(&format!("{trial}/agent/scenes/turn-001.tar"), "fake tar");
        let path = f.root.join(trial);
        let output = f.run(
            &[
                "branch-from",
                path.to_str().unwrap(),
                "1",
                "--times",
                "1",
                "--no-build",
            ],
            &[("MOCK_RESTORE_EXIT", restore_exit)],
        );
        let log = fs::read_to_string(f.root.join("mock.log")).unwrap();
        if restore_exit == "0" {
            success(&output);
            assert!(log.find("--resume ").unwrap() < log.find("harness/tasks/a/tests ").unwrap());
        } else {
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("incomplete branch evaluation")
            );
            assert!(!log.contains("--resume "));
            assert!(!log.contains("harness/tasks/a/tests "));
        }
    }
}
