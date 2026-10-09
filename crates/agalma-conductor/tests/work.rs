//! Deterministic integration tests for the M1 queue-driven `agalma work` loop.
//!
//! Each test runs against its own scratch origin repository under
//! `target/test-runs/work-*`: a real `git` origin seeded with a small source
//! file and a real isolated `bd` store. Model calls are replaced by the
//! conductor's scripted activity mode (`AGALMA_ACTIVITY_MODE=scripted`); the
//! acceptance command list still runs, deterministically, via the in-process
//! runner. No `/tmp`, no network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

fn fresh(name: &str) -> PathBuf {
    let dir = root().join("target").join("test-runs").join(name);
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn run_ok(program: &str, dir: &Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        stderr_of(&out)
    );
}

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for these tests but was not found");
}

/// A scratch origin repo (git + isolated bd store) with one task issue.
struct Scratch {
    origin: PathBuf,
    state_dir: PathBuf,
}

impl Scratch {
    /// `acceptance` is the fenced-block acceptance list body (already indented).
    fn new(name: &str, acceptance: &str, max_attempts: u32) -> (Scratch, String) {
        require_bd();
        let dir = fresh(&format!("work-{name}"));
        let origin = dir.join("origin");
        fs::create_dir_all(origin.join("src")).expect("origin src");
        fs::write(
            origin.join("src").join("lib.rs"),
            "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
        )
        .expect("seed source");

        run_ok("git", &origin, &["init", "-q", "-b", "main"]);
        run_ok(
            "git",
            &origin,
            &["config", "user.email", "agalma@localhost"],
        );
        run_ok("git", &origin, &["config", "user.name", "Agalma"]);
        run_ok("git", &origin, &["add", "-A"]);
        run_ok("git", &origin, &["commit", "-qm", "seed"]);

        // bd initializes a local store because cwd is the scratch repo.
        let prefix = name.replace('-', "");
        let out = Command::new("bd")
            .args([
                "init",
                "--prefix",
                &prefix,
                "--non-interactive",
                "--skip-agents",
                "--skip-hooks",
            ])
            .current_dir(&origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd init");
        assert!(out.status.success(), "bd init: {}", stderr_of(&out));

        let description = format!(
            "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n{acceptance}\npriority: normal\nfamily: fix\nmax_attempts: {max_attempts}\n```\n"
        );
        let scratch = Scratch {
            origin,
            state_dir: dir.join("state"),
        };
        let id = scratch.create("Scratch task", &description);
        (scratch, id)
    }

    fn beads(&self) -> PathBuf {
        self.origin.join(".beads")
    }

    fn bd(&self, args: &[&str]) -> Output {
        Command::new("bd")
            .arg("--db")
            .arg(self.beads())
            .args(args)
            .current_dir(&self.origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd")
    }

    fn create(&self, title: &str, description: &str) -> String {
        let out = self.bd(&[
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            description,
            "--json",
        ]);
        assert!(out.status.success(), "bd create: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("bd create json");
        value["id"].as_str().expect("created id").to_string()
    }

    fn show(&self, id: &str) -> Value {
        let out = self.bd(&["show", id, "--json", "--include-comments"]);
        assert!(out.status.success(), "bd show: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("bd show json");
        value
            .as_array()
            .expect("show array")
            .first()
            .expect("one issue")
            .clone()
    }

    fn work(&self, once: bool) -> Output {
        self.work_with(once, None)
    }

    fn work_with(&self, once: bool, max_tasks: Option<u32>) -> Output {
        let mut command = Command::new(bin());
        command.args([
            "work",
            "--repo",
            self.origin.to_str().expect("origin utf8"),
            "--state-dir",
            self.state_dir.to_str().expect("state utf8"),
        ]);
        if once {
            command.arg("--once");
        }
        if let Some(max) = max_tasks {
            command.arg("--max-tasks").arg(max.to_string());
        }
        command
            .env("AGALMA_ACTIVITY_MODE", "scripted")
            .output()
            .expect("run agalma work")
    }

    /// Create another task issue with an explicit priority and acceptance.
    fn create_task(&self, title: &str, priority: &str, acceptance: &str) -> String {
        let description = format!(
            "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n{acceptance}\npriority: {priority}\nfamily: fix\nmax_attempts: 2\n```\n"
        );
        self.create(title, &description)
    }

    fn git_origin(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.origin)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

fn comments_text(issue: &Value) -> String {
    issue["comments"]
        .as_array()
        .map(|comments| {
            comments
                .iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------

#[test]
fn happy_path_closes_task_and_merges_origin() {
    let (scratch, id) = Scratch::new(
        "happy",
        "  - sh -c 'test -f src/lib.rs'\n  - sh -c 'grep -q 42 src/lib.rs'",
        2,
    );

    let out = scratch.work(true);
    assert!(
        out.status.success(),
        "work --once should succeed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        stderr_of(&out)
    );

    // Projection: phase comments with the execution id, and the task closed.
    let issue = scratch.show(&id);
    assert_eq!(issue["status"], "closed", "issue closed on done: {issue}");
    let text = comments_text(&issue);
    let execution = format!("exec:{id}:1");
    for phase in ["intake", "checkout", "build", "verify", "integrate", "done"] {
        assert!(
            text.contains(&format!("phase={phase}")),
            "missing phase={phase} comment: {text}"
        );
    }
    assert!(
        text.contains(&format!("execution={execution}")),
        "execution id projected: {text}"
    );

    // Integration: origin main advanced to the candidate and is tagged m1/<exec>.
    let main = scratch.git_origin(&["rev-parse", "refs/heads/main"]);
    let seed = scratch.git_origin(&["rev-parse", "refs/heads/main~1"]);
    assert_ne!(main, seed, "origin main advanced");
    let tag = format!("m1/exec-{id}-1");
    assert_eq!(
        scratch.git_origin(&["rev-parse", &format!("refs/tags/{tag}")]),
        main,
        "m1 tag points at merged main"
    );
}

#[test]
fn red_acceptance_retries_then_parks() {
    let (scratch, id) = Scratch::new("red", "  - sh -c 'grep -q MAGIC src/lib.rs'", 2);
    let before = scratch.git_origin(&["rev-parse", "refs/heads/main"]);

    let out = scratch.work(true);
    assert!(
        !out.status.success(),
        "--once exits nonzero on park: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let issue = scratch.show(&id);
    let labels: Vec<&str> = issue["labels"]
        .as_array()
        .map(|l| l.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(labels.contains(&"parked"), "parked label: {issue}");
    let text = comments_text(&issue);
    assert!(
        text.contains("max_attempts"),
        "park reason cites the attempt bound: {text}"
    );

    // Two attempts ran (bounded retry), and the origin was never merged.
    assert_eq!(
        scratch.git_origin(&["rev-parse", "refs/heads/main"]),
        before,
        "origin main unchanged on red"
    );
    assert!(
        scratch
            .git_origin(&["tag", "-l", &format!("m1/exec-{id}-1")])
            .is_empty(),
        "no integration tag on red"
    );

    // The failed acceptance output is captured as an artifact.
    let run_root = scratch.state_dir.join("runs").join(format!("exec-{id}-1"));
    let failure = run_root.join("artifacts").join("failure@1.txt");
    assert!(
        failure.exists(),
        "failure artifact captured: {}",
        failure.display()
    );
    let captured = fs::read_to_string(&failure).expect("read failure");
    assert!(
        captured.contains("MAGIC"),
        "captured failing command: {captured}"
    );
}

#[test]
fn triage_picks_higher_priority_and_max_tasks_bounds() {
    let (scratch, normal_id) = Scratch::new("triage", "  - sh -c 'grep -q 42 src/lib.rs'", 2);
    let critical_id =
        scratch.create_task("Critical", "critical", "  - sh -c 'grep -q 42 src/lib.rs'");

    // Two eligible tasks → static `triage.pick-next` picks the critical one;
    // `--max-tasks 1` bounds the loop to a single task.
    let out = scratch.work_with(false, Some(1));
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(
        scratch.show(&critical_id)["status"],
        "closed",
        "critical task chosen by triage"
    );
    assert_eq!(
        scratch.show(&normal_id)["status"],
        "open",
        "--max-tasks bounds the loop"
    );
}

#[test]
fn generation_increments_on_rerun() {
    let (scratch, id) = Scratch::new("generation", "  - sh -c 'grep -q 42 src/lib.rs'", 2);

    let first = scratch.work(true);
    assert!(first.status.success(), "first run: {}", stderr_of(&first));

    // Reopen the closed issue and run again: the execution generation advances.
    let reopened = scratch.bd(&["reopen", &id]);
    assert!(
        reopened.status.success(),
        "reopen: {}",
        stderr_of(&reopened)
    );
    let second = scratch.work(true);
    assert!(
        second.status.success(),
        "second run: {}",
        stderr_of(&second)
    );

    let issue = scratch.show(&id);
    let text = comments_text(&issue);
    assert!(
        text.contains(&format!("execution=exec:{id}:1")),
        "generation 1 recorded: {text}"
    );
    assert!(
        text.contains(&format!("execution=exec:{id}:2")),
        "generation 2 recorded: {text}"
    );
}
