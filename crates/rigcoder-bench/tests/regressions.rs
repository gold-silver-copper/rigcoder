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
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
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
        fixture.write(
            "harness/build-inputs.py",
            include_str!("../../../harness/build-inputs.py"),
        );
        fixture.script("harness/write-receipt.sh", r#"#!/bin/sh
set -eu
receipt_tmp=$(mktemp -d)
trap 'rm -rf "$receipt_tmp"' EXIT
python3 harness/build-inputs.py snapshot "$PWD" "$receipt_tmp/src" > "$receipt_tmp/source.json"
python3 harness/build-inputs.py receipt "$receipt_tmp/source.json" "$PWD/harness/bin/rigcoder-linux-$ARCH" "$ARCH" sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa > "harness/bin/rigcoder-linux-$ARCH.build.json"
"#);
        fixture.script(
            "harness/build-linux.sh",
            "#!/bin/sh\nmkdir -p harness/bin\ncp crates/rigcoder/src/prompt.md harness/bin/rigcoder-linux-$ARCH\nbash harness/write-receipt.sh\n",
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
    esac
    while [ "$#" -gt 0 ]; do
        if [ "$1" = --iidfile ]; then
            printf '%s\n' "${MOCK_IMAGE_ID:-sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef}" > "$2"
            break
        fi
        shift
    done ;;
cp)
    case "$3" in
    *:/usr/local/bin/rigcoder) cat "$2" > "$MOCK_STATE"; printf 'upload:%s\n' "$(cat "$2")" >> "$MOCK_LOG" ;;
    esac ;;
exec)
    case "$*" in
    *--task-file*) exit "${MOCK_AGENT_EXIT:-0}" ;;
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
fn self_improvement_cannot_select_holdout_tasks_through_dev_or_include() {
    for included in [false, true] {
        let f = Fixture::new();
        if !included {
            f.write("harness/slices/dev.txt", "a\nheldout\n");
            f.git(&["add", "harness/slices/dev.txt"]);
            f.git(&["commit", "-qm", "overlapping fixture slices"]);
        }
        let before = f.git(&["rev-parse", "HEAD"]);
        let mut args = vec!["iterate", "--generations", "1", "--holdout", "false"];
        if included {
            args.extend(["--include", "heldout"]);
        }
        let output = f.run(&args, &[]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("overlap the holdout"));
        assert_eq!(f.git(&["rev-parse", "HEAD"]), before);
        assert_eq!(f.git(&["branch", "--show-current"]), "main");
        assert!(!f.root.join("mock.log").exists());
        assert!(f.ledger().is_empty());
    }
}

#[test]
fn invalid_task_limits_never_start_builds_or_trials() {
    for section in ["agent", "verifier", "environment"] {
        for value in ["0.0", "-1.0", "0.5", "nan", "inf"] {
            let f = Fixture::new();
            let key = if section == "environment" {
                "build_timeout_sec"
            } else {
                "timeout_sec"
            };
            f.write(
                "harness/tasks/a/task.toml",
                &format!("[{section}]\n{key} = {value}\n"),
            );
            let output = f.run(&["run", "--no-build"], &[]);
            assert!(!output.status.success(), "accepted {section}/{value}");
            assert!(f.ledger().is_empty());
            let log = fs::read_to_string(f.root.join("mock.log")).unwrap();
            assert!(
                !log.lines()
                    .any(|line| line.starts_with("build ") || line.starts_with("run "))
            );
        }
    }
}

#[test]
fn invalid_receipt_paths_cannot_be_treated_as_missing() {
    for link in [false, true] {
        let f = Fixture::new();
        let receipt = f.root.join(format!(
            "harness/bin/rigcoder-linux-{}.build.json",
            std::env::consts::ARCH
        ));
        if link {
            std::os::unix::fs::symlink("missing-receipt", &receipt).unwrap();
        } else {
            fs::create_dir(&receipt).unwrap();
        }
        let result = f.run(&["run", "--no-build"], &[]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("receipt must be a regular file"));
        assert!(f.ledger().is_empty());
    }
}

#[test]
fn evaluation_validates_receipts_before_trials_and_rejects_stale_inputs() {
    let f = Fixture::new();
    success(&f.run(&["run"], &[]));
    let ledger = f.ledger();
    let job = PathBuf::from(ledger[0]["job_dir"].as_str().unwrap());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(job.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["binary"]["source_binding"], "matched_receipt");
    let before = fs::read_to_string(f.root.join("mock.log")).unwrap();
    f.write(PROMPT, "changed source\n");
    let stale = f.run(&["run", "--no-build"], &[]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("source inputs mismatch"));
    f.write(PROMPT, "baseline\n");
    f.write(
        &format!("harness/bin/rigcoder-linux-{}", std::env::consts::ARCH),
        "different binary",
    );
    let stale = f.run(&["run", "--no-build"], &[]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("binary mismatch"));
    assert_eq!(f.ledger().len(), 1);
    let after = fs::read_to_string(f.root.join("mock.log")).unwrap();
    assert_eq!(
        before
            .lines()
            .filter(|line| line.starts_with("run "))
            .count(),
        after
            .lines()
            .filter(|line| line.starts_with("run "))
            .count()
    );
}

#[test]
fn trials_launch_the_recorded_image_id_and_reject_invalid_receipts() {
    let f = Fixture::new();
    success(&f.run(&["run", "--no-build"], &[]));
    let ledger = f.ledger();
    let job = PathBuf::from(ledger[0]["job_dir"].as_str().unwrap());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(job.join("manifest.json")).unwrap()).unwrap();
    let image = manifest["images"]["a"].as_str().unwrap();
    assert!(image.starts_with("sha256:"));
    let log = fs::read_to_string(f.root.join("mock.log")).unwrap();
    let launches: Vec<_> = log
        .lines()
        .filter(|line| line.starts_with("run "))
        .collect();
    assert_eq!(launches.len(), 2);
    assert!(
        launches
            .iter()
            .all(|line| line.contains(image) && !line.contains("rigcoder-bench/"))
    );
    let invalid = Fixture::new();
    assert!(
        !invalid
            .run(&["run", "--no-build"], &[("MOCK_IMAGE_ID", "latest")])
            .status
            .success()
    );
    assert!(invalid.ledger().is_empty());
    assert!(
        !fs::read_to_string(invalid.root.join("mock.log"))
            .unwrap()
            .lines()
            .any(|line| line.starts_with("run "))
    );
}

#[test]
fn evaluation_uses_frozen_task_inputs_when_the_original_changes() {
    let f = Fixture::new();
    let empty = f.root.join("harness/tasks/a/environment/empty");
    fs::create_dir(&empty).unwrap();
    fs::set_permissions(&empty, fs::Permissions::from_mode(0o750)).unwrap();
    let docker = fs::read_to_string(f.root.join("fakebin/docker")).unwrap();
    f.script(
        "fakebin/docker",
        &docker.replace(
            "cat \"$2\" > \"$MOCK_STATE\";",
            "cat \"$2\" > \"$MOCK_STATE\"; printf 'different task\\n' > \"$MUTATE_TASK\";",
        ),
    );
    let instruction = f.root.join("harness/tasks/a/instruction.md");
    let output = f.run(
        &["run", "--no-build"],
        &[("MUTATE_TASK", instruction.to_str().unwrap())],
    );
    success(&output);
    let ledger = f.ledger();
    let job = PathBuf::from(ledger[0]["job_dir"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(job.join("tasks/a/instruction.md")).unwrap(),
        "solve it"
    );
    assert_eq!(fs::read_to_string(instruction).unwrap(), "different task\n");
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(job.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["task_files"]["a/tests/test.sh"]["git_blob_oid"].is_string());
    let copied = job.join("tasks/a/environment/empty");
    assert!(copied.is_dir());
    assert_eq!(
        fs::metadata(&copied).unwrap().permissions().mode() & 0o777,
        0o750
    );
    let entry = &manifest["task_files"]["a/environment/empty"];
    assert_eq!(entry["kind"], "directory");
    assert_eq!(entry["mode"].as_u64().unwrap() & 0o777, 0o750);
}

#[test]
fn iteration_cannot_publish_a_development_candidate() {
    let f = Fixture::new();
    let before = f.git(&["rev-parse", "HEAD"]);
    let output = f.run(&["iterate", "--pr"], &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--pr'"));
    assert_eq!(f.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(f.git(&["branch", "--show-current"]), "main");
    assert!(!f.root.join("mock.log").exists());
    assert!(f.ledger().is_empty());
}

#[test]
fn missing_usage_is_unknown_in_trial_and_summary_artifacts() {
    let f = Fixture::new();
    success(&f.run(&["run", "--no-build"], &[]));
    let ledger = f.ledger();
    let job = PathBuf::from(ledger[0]["job_dir"].as_str().unwrap());
    let trial: Value =
        serde_json::from_str(&fs::read_to_string(job.join("a__1/result.json")).unwrap()).unwrap();
    for field in ["input_tokens", "output_tokens", "cache_tokens"] {
        assert!(trial[field].is_null(), "missing trial {field} is unknown");
        assert!(
            ledger[0]["cost"][field].is_null(),
            "missing total {field} is unknown"
        );
    }
}

#[test]
fn evaluation_uses_one_recorded_binary_even_if_the_original_changes() {
    let f = Fixture::new();
    let binary = format!("harness/bin/rigcoder-linux-{}", std::env::consts::ARCH);
    let expected = f.git(&["hash-object", "--no-filters", "--", &binary]);
    let docker = fs::read_to_string(f.root.join("fakebin/docker")).unwrap();
    f.script(
        "fakebin/docker",
        &docker.replace(
            "cat \"$2\" > \"$MOCK_STATE\";",
            "cat \"$2\" > \"$MOCK_STATE\"; printf 'candidate\\n' > \"$MUTATE_BINARY\";",
        ),
    );
    let original = f.root.join(&binary);
    success(&f.run(
        &["run", "--no-build"],
        &[("MUTATE_BINARY", original.to_str().unwrap())],
    ));
    let ledger = f.ledger();
    assert_eq!(
        ledger[0]["score"], 1.0,
        "all tasks must execute the same bytes"
    );
    let job = PathBuf::from(ledger[0]["job_dir"].as_str().unwrap());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(job.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["binary"]["git_blob_oid"], expected);
    assert_eq!(manifest["binary"]["source_binding"], "unverified");
    assert_eq!(
        fs::read_to_string(job.join("rigcoder.bin")).unwrap(),
        "baseline\n"
    );
    assert_eq!(fs::read_to_string(original).unwrap(), "candidate\n");
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
fn rejection_restores_the_binary_without_a_holdout_run() {
    let f = Fixture::new();
    success(&f.run(
        &["iterate", "--generations", "2", "--holdout", "false"],
        &[],
    ));
    assert_eq!(f.ledger()[1]["decision"], "reverted");
    let binary = f.root.join(format!(
        "harness/bin/rigcoder-linux-{}",
        std::env::consts::ARCH
    ));
    assert_eq!(fs::read_to_string(binary).unwrap(), "baseline\n");
    // A subsequent evaluation-only invocation must execute the kept behavior.
    success(&f.run(&["run", "--no-build"], &[]));
    assert_eq!(f.ledger().last().unwrap()["score"], 1.0);
}

#[test]
fn failed_rebuild_cannot_leave_a_rejected_binary_available() {
    let f = Fixture::new();
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
git add crates/rigcoder/src/prompt.md "$note"
"#,
    );
    f.script(
        "harness/build-linux.sh",
        "#!/bin/sh\nif [ -f harness/bin/build-count ]; then count=$(cat harness/bin/build-count); else count=0; fi\ncount=$((count + 1))\necho $count > harness/bin/build-count\ncp crates/rigcoder/src/prompt.md harness/bin/rigcoder-linux-$ARCH\nbash harness/write-receipt.sh\n[ $count -lt 3 ]\n",
    );
    f.git(&["add", "harness/build-linux.sh"]);
    f.git(&["commit", "-qm", "fail rollback build"]);
    let output = f.run(
        &["iterate", "--generations", "2", "--holdout", "false"],
        &[],
    );
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(f.root.join(PROMPT)).unwrap(),
        "baseline\n"
    );
    let binary = f.root.join(format!(
        "harness/bin/rigcoder-linux-{}",
        std::env::consts::ARCH
    ));
    assert!(!binary.exists(), "a failed build must not look usable");
    assert_eq!(f.ledger()[1]["decision"], "reverted");
    let ledger = f.ledger();
    let rejected = PathBuf::from(ledger[1]["job_dir"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(rejected.join("improvement.md")).unwrap(),
        "candidate rationale\n"
    );
    assert_eq!(f.git(&["diff", "--name-only"]), "harness/ledger.jsonl");
    assert!(f.git(&["diff", "--cached", "--name-only"]).is_empty());
    assert!(
        f.git(&["ls-files", "--others", "--exclude-standard"])
            .is_empty()
    );
    assert!(!f.run(&["run", "--no-build"], &[]).status.success());
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
    let tests = log
        .find(&format!(
            "{}/tasks/a/tests ",
            ledger[0]["job_dir"].as_str().unwrap()
        ))
        .unwrap();
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
fn failed_improvement_steps_restore_source_and_archive_notes() {
    for failed_agent in [true, false] {
        let f = Fixture::new();
        f.script(
            "fakebin/agent",
            r#"#!/bin/sh
printf 'candidate\n' > crates/rigcoder/src/prompt.md
git add crates/rigcoder/src/prompt.md
while [ "$#" -gt 0 ]; do
    if [ "$1" = --allow ]; then
        case "$2" in */harness/notes/*)
            printf 'failed improvement\n' > "$2"
            git add "$2"
            ;;
        esac
    fi
    shift
done
exit "${MOCK_META_EXIT:-0}"
"#,
        );
        if !failed_agent {
            f.script("fakebin/cargo", "#!/bin/sh\nexit 1\n");
        }
        let output = f.run(
            &["iterate", "--generations", "2", "--holdout", "false"],
            &[("MOCK_META_EXIT", if failed_agent { "2" } else { "0" })],
        );
        assert!(!output.status.success());
        assert_eq!(
            fs::read_to_string(f.root.join(PROMPT)).unwrap(),
            "baseline\n"
        );
        assert!(f.git(&["diff", "--cached"]).is_empty());
        assert_eq!(f.ledger().len(), 1);
        let archived: Vec<_> = fs::read_dir(f.root.join("harness/runs"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("improvement.md"))
            .filter(|path| path.is_file())
            .collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(fs::read(&archived[0]).unwrap(), b"failed improvement\n");
    }
}

#[test]
fn unscored_candidate_failures_restore_source_and_preserve_staged_notes() {
    for build_failure in [true, false] {
        let f = Fixture::new();
        let agent = fs::read_to_string(f.root.join("fakebin/agent")).unwrap();
        f.script(
            "fakebin/agent",
            &(agent
                + r#"
while [ "$#" -gt 0 ]; do
    if [ "$1" = --allow ]; then
        case "$2" in */harness/notes/*)
            printf 'unscored note\n' > "$2"
            git add "$2"
            rm "$2"
            ;;
        esac
    fi
    shift
done
"#),
        );
        if build_failure {
            f.script(
                "harness/build-linux.sh",
                r#"#!/bin/sh
mkdir -p harness/bin
cp crates/rigcoder/src/prompt.md harness/bin/rigcoder-linux-$ARCH
if grep -q candidate crates/rigcoder/src/prompt.md; then exit 1; fi
bash harness/write-receipt.sh
"#,
            );
        } else {
            let docker = fs::read_to_string(f.root.join("fakebin/docker")).unwrap();
            f.script("fakebin/docker", &docker.replace(
            "*'bash /tests/test.sh'*)",
            "*'bash /tests/test.sh'*) if [ \"$(cat \"$MOCK_STATE\")\" = candidate ]; then exit 2; fi;",
        ));
        }
        f.git(&["add", "harness/build-linux.sh"]);
        f.git(&[
            "commit",
            "--allow-empty",
            "-qm",
            "candidate build failure fixture",
        ]);
        let output = f.run(
            &["iterate", "--generations", "2", "--holdout", "false"],
            &[],
        );
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(if build_failure {
                "build-linux.sh failed"
            } else {
                "verifier failed"
            })
        );
        assert_eq!(
            fs::read_to_string(f.root.join(PROMPT)).unwrap(),
            "baseline\n"
        );
        assert!(f.git(&["diff", "--cached"]).is_empty());
        assert!(
            !f.root
                .join(format!(
                    "harness/bin/rigcoder-linux-{}",
                    std::env::consts::ARCH
                ))
                .exists()
        );
        assert_eq!(f.ledger().len(), 1);
        let archived: Vec<_> = fs::read_dir(f.root.join("harness/runs"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("improvement-index.md"))
            .filter(|path| path.is_file())
            .collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(fs::read(&archived[0]).unwrap(), b"unscored note\n");
        assert!(
            !f.root
                .join(".git/rigcoder-pending-improvement.json")
                .exists()
        );
    }
}

#[test]
fn a_reported_inner_timeout_still_allows_verification() {
    let f = Fixture::new();
    success(&f.run(&["run", "--no-build"], &[("MOCK_AGENT_EXIT", "137")]));
    let log = fs::read_to_string(f.root.join("mock.log")).unwrap();
    assert!(log.contains("bash /tests/test.sh"));
    assert!(log.contains("cat /logs/verifier/reward.txt"));
    assert_eq!(f.ledger().len(), 1);
}

#[test]
fn host_deadline_stops_execution_that_ignores_the_container_timeout() {
    let f = Fixture::new();
    // Use actual GNU timeout, not the fixture's pass-through substitute.
    let timeout = Command::new("sh")
        .args(["-c", "for name in timeout gtimeout; do if \"$name\" --version 2>/dev/null | grep -q 'GNU coreutils'; then command -v \"$name\"; exit 0; fi; done; exit 1"])
        .output()
        .unwrap();
    assert!(
        timeout.status.success(),
        "GNU timeout is required for this test"
    );
    fs::remove_file(f.root.join("fakebin/timeout")).unwrap();
    std::os::unix::fs::symlink(
        String::from_utf8(timeout.stdout).unwrap().trim(),
        f.root.join("fakebin/timeout"),
    )
    .unwrap();
    f.write("harness/slices/dev.txt", "a\n");
    f.write("harness/tasks/a/task.toml", "[agent]\ntimeout_sec = 1\n");
    let docker = fs::read_to_string(f.root.join("fakebin/docker")).unwrap();
    f.script(
        "fakebin/docker",
        &docker.replace(
            "exec)\n",
            "exec)\n    case \"$*\" in *--task-file*) sleep 3; echo escaped-deadline >> \"$MOCK_LOG\"; exit 0 ;; esac\n",
        ),
    );
    let output = f.run(&["run", "--no-build"], &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("docker client terminated by signal"));
    assert!(f.ledger().is_empty());
    let log = fs::read_to_string(f.root.join("mock.log")).unwrap();
    assert!(
        !log.contains("escaped-deadline"),
        "execution escaped its deadline: {log}"
    );
    assert!(!log.contains("bash /tests/test.sh"));
    assert!(!log.contains("cat /logs/verifier/reward.txt"));
    assert!(log.lines().any(|line| line.starts_with("rm -f ")));
}

#[test]
fn a_missing_host_timeout_fails_before_starting_docker() {
    let f = Fixture::new();
    fs::remove_file(f.root.join("fakebin/timeout")).unwrap();
    // Keep the Git prerequisite available while excluding both timeout names.
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    std::os::unix::fs::symlink(git, f.root.join("fakebin/git")).unwrap();
    let output = f.run(
        &["run", "--no-build"],
        &[("PATH", f.root.join("fakebin").to_str().unwrap())],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("GNU timeout is required"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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

#[test]
fn killed_improver_leaves_recovery_evidence_and_blocks_reuse() {
    let f = Fixture::new();
    f.script(
        "fakebin/agent",
        "#!/bin/sh\nprintf 'interrupted candidate\\n' > crates/rigcoder/src/prompt.md\nkill -KILL \"$PPID\"\n",
    );
    let baseline = f.git(&["rev-parse", "HEAD"]);
    let output = f.run(
        &["iterate", "--generations", "2", "--holdout", "false"],
        &[],
    );
    assert!(!output.status.success());
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(output.status.signal(), Some(9));
    let marker = f.root.join(".git/rigcoder-pending-improvement.json");
    let record: Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert_eq!(record["baseline"], baseline);
    assert_eq!(
        fs::read_to_string(f.root.join(PROMPT)).unwrap(),
        "interrupted candidate\n"
    );
    let log = fs::read(f.root.join("mock.log")).unwrap();
    for args in [
        vec!["run", "--no-build"],
        vec!["iterate", "--generations", "1"],
    ] {
        let output = f.run(&args, &[]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unfinished self-improvement"));
        assert_eq!(fs::read(f.root.join("mock.log")).unwrap(), log);
        assert!(marker.exists());
    }
    assert_eq!(f.ledger().len(), 1); // Only the completed baseline has a decision.
}

#[test]
fn completed_candidate_decisions_clear_the_interruption_marker() {
    for unchanged in [false, true] {
        let f = Fixture::new();
        if unchanged {
            f.script("fakebin/agent", "#!/bin/sh\nexit 0\n");
        }
        success(&f.run(
            &["iterate", "--generations", "2", "--holdout", "false"],
            &[],
        ));
        assert_eq!(
            f.ledger()[1]["decision"],
            if unchanged { "tie" } else { "reverted" }
        );
        assert!(
            !f.root
                .join(".git/rigcoder-pending-improvement.json")
                .exists()
        );
    }
}

#[test]
fn interruption_marker_is_local_to_the_linked_worktree() {
    let f = Fixture::new();
    let linked = f.root.join("linked");
    f.git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
    let linked = Fixture { root: linked };
    let marker = linked.root.join(linked.git(&[
        "rev-parse",
        "--git-path",
        "rigcoder-pending-improvement.json",
    ]));
    fs::write(&marker, b"interrupted marker").unwrap();
    let output = linked.run(&["run", "--no-build"], &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unfinished self-improvement"));
    assert!(marker.exists());
    assert!(
        !f.root
            .join(".git/rigcoder-pending-improvement.json")
            .exists()
    );
    success(&f.run(&["run", "--no-build"], &[]));
}

#[test]
fn output_line_trials_use_host_scoring_and_keep_capture_evidence() {
    for present in [false, true] {
        let f = Fixture::new();
        for task in ["a", "b"] {
            f.write(
                &format!("harness/tasks/{task}/tests/output-line.json"),
                r#"{"artifact":"/app/answer.txt","expected":"SYNTHETIC"}"#,
            );
        }
        let response = f.root.join("response.http");
        let generated = Command::new("python3").args(["-I", "-c", r#"
import io, sys, tarfile
if sys.argv[2] == 'true':
    target = io.BytesIO()
    with tarfile.open(fileobj=target, mode='w', format=tarfile.USTAR_FORMAT) as tar:
        info = tarfile.TarInfo('answer.txt'); info.size = 10
        tar.addfile(info, io.BytesIO(b'SYNTHETIC\n'))
    body = target.getvalue(); status = '200 OK'
else:
    body = b''; status = '404 Not Found'
with open(sys.argv[1], 'wb') as output:
    output.write(('HTTP/1.1 ' + status + '\r\nContent-Length: ' + str(len(body)) + '\r\n\r\n').encode() + body)
"#]).arg(&response).arg(present.to_string()).output().unwrap();
        success(&generated);
        let docker = fs::read_to_string(f.root.join("fakebin/docker")).unwrap();
        f.script("fakebin/docker", &docker.replace("case \"$1\" in\nversion)",
            "case \"$1\" in\nstop) ;;\ninspect) echo false ;;\nsystem) cat >/dev/null; cat \"$MOCK_RESPONSE\" ;;\nversion)"));
        success(&f.run(
            &["run", "--no-build"],
            &[("MOCK_RESPONSE", response.to_str().unwrap())],
        ));
        assert_eq!(f.ledger()[0]["score"], if present { 1.0 } else { 0.0 });
        let log = fs::read_to_string(f.root.join("mock.log")).unwrap();
        assert!(!log.contains("bash /tests/test.sh"));
        assert!(!log.contains("/tests/output-line.json"));
        let job = fs::read_dir(f.root.join("harness/runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let evidence = job.join("a__1/verifier");
        let record: Value =
            serde_json::from_slice(&fs::read(evidence.join("output-line-result.json")).unwrap())
                .unwrap();
        assert_eq!(record["artifact_missing"], !present);
        assert_eq!(evidence.join("artifact.tar").exists(), present);
    }
}

#[test]
fn malformed_output_capture_cannot_record_an_acceptance_decision() {
    let f = Fixture::new();
    for task in ["a", "b"] {
        f.write(
            &format!("harness/tasks/{task}/tests/output-line.json"),
            r#"{"artifact":"/app/answer.txt","expected":"SYNTHETIC"}"#,
        );
    }
    let docker = fs::read_to_string(f.root.join("fakebin/docker")).unwrap();
    f.script("fakebin/docker", &docker.replace("case \"$1\" in\nversion)",
        "case \"$1\" in\nstop) ;;\ninspect) echo false ;;\nsystem) cat >/dev/null; printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 7\\r\\n\\r\\ninvalid' ;;\nversion)"));
    let output = f.run(
        &[
            "iterate",
            "--no-improve",
            "--no-build",
            "--generations",
            "1",
            "--holdout",
            "false",
        ],
        &[],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("output-line scorer failed"));
    assert!(f.ledger().is_empty());
    let job = fs::read_dir(f.root.join("harness/runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(job.join("error.txt").is_file());
    assert!(!job.join("summary.json").exists());
    assert_eq!(
        fs::read(job.join("a__1/verifier/artifact.tar")).unwrap(),
        b"invalid"
    );
    assert!(!job.join("a__1/verifier/output-line-result.json").exists());
}
