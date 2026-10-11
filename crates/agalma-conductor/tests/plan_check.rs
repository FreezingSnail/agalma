//! `agalma plan-check` (CP.1, `agalma-i5h.2`) end-to-end tests.
//!
//! Each test runs the real `agalma` binary against its own scratch bd workspace
//! under `target/test-runs/plan-check-*` (never `/tmp`): `git init` + `bd init
//! --skip-agents --skip-hooks`. Children are created `--defer`red with the
//! `staged,difficulty:*` labels so the CLI's `--all` query is exercised.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

static RUN_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn fresh(name: &str) -> PathBuf {
    let n = RUN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = root()
        .join("target")
        .join("test-runs")
        .join(format!("plan-check-{name}-{}-{n}", std::process::id()));
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn run_ok(program: &str, dir: &Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {args:?}: {e}"));
    assert!(out.status.success(), "{program} {args:?}: {}", stderr_of(&out));
}

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for these tests but was not found");
}

/// An isolated bd workspace under `target/test-runs/`.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        require_bd();
        let dir = fresh(name);
        run_ok("git", &dir, &["init", "-q"]);
        run_ok("git", &dir, &["config", "user.email", "agalma@localhost"]);
        run_ok("git", &dir, &["config", "user.name", "Agalma"]);

        let out = Command::new("bd")
            .args([
                "init",
                "--prefix",
                name,
                "--non-interactive",
                "--skip-agents",
                "--skip-hooks",
            ])
            .current_dir(&dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd init");
        assert!(out.status.success(), "bd init: {}", stderr_of(&out));

        Scratch { dir }
    }

    fn repo(&self) -> &Path {
        &self.dir
    }

    fn beads_dir(&self) -> PathBuf {
        self.dir.join(".beads")
    }

    fn bd(&self, args: &[&str]) -> std::process::Output {
        Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args(args)
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd")
    }

    fn create_epic(&self, title: &str) -> String {
        let out = self.bd(&[
            "create",
            title,
            "-t",
            "epic",
            "-p",
            "2",
            "--description",
            "epic",
            "--json",
        ]);
        assert!(out.status.success(), "bd create epic: {}", stderr_of(&out));
        let value: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("bd create --json");
        value["id"].as_str().expect("epic id").to_string()
    }

    /// Create a deferred, staged child with an explicit label list and design.
    fn create_child(&self, parent: &str, title: &str, description: &str, labels: &str) -> String {
        let out = self.bd(&[
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--parent",
            parent,
            "--description",
            description,
            "--design",
            "Small, single-file change; no new interface.",
            "--labels",
            labels,
            "--defer",
            "2099-01-01",
            "--json",
        ]);
        assert!(out.status.success(), "bd create child: {}", stderr_of(&out));
        let value: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("bd create --json");
        value["id"].as_str().expect("child id").to_string()
    }

    fn dep_add(&self, child: &str, depends_on: &str) {
        let out = self.bd(&["dep", "add", child, depends_on]);
        assert!(out.status.success(), "bd dep add: {}", stderr_of(&out));
    }

    fn comment(&self, id: &str, text: &str) {
        let out = self.bd(&["comment", id, text]);
        assert!(out.status.success(), "bd comment: {}", stderr_of(&out));
    }
}

/// A description that satisfies every rule at low tier (the fence lives under
/// `# MACHINE`, so it is excluded from the prose cap).
fn low_desc(id: &str) -> String {
    format!(
        "# Scope\nDo exactly one small thing for {id}.\n\n\
         # Files\ncrates/foo/src/{id}.rs\n\n\
         # Contract\nRun the acceptance command.\n\n\
         # Invariants\nNo behavior change outside scope.\n\n\
         # Non-goals\nNo refactor.\n\n\
         # MACHINE\nprovides:\nconsumes:\nfiles: crates/foo/src/{id}.rs\ntier: low\n\n\
         ```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - cargo test -p foo\nbudget_usd: 2.00\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n"
    )
}

fn run_plan_check(epic: &str, repo: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agalma"));
    cmd.arg("plan-check")
        .arg("--epic")
        .arg(epic)
        .arg("--repo")
        .arg(repo)
        .args(extra)
        .env("GIT_TERMINAL_PROMPT", "0");
    cmd.output().expect("run agalma plan-check")
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn a_valid_decomposition_reports_ok() {
    let scratch = Scratch::new("ok");
    let epic = scratch.create_epic("Epic");
    for i in 0..8 {
        scratch.create_child(
            &epic,
            &format!("child {i}"),
            &low_desc(&format!("c{i}")),
            "staged,difficulty:low",
        );
    }

    let out = run_plan_check(&epic, scratch.repo(), &[]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "OK");
}

#[test]
fn a_bad_decomposition_lists_rule_names_and_fails() {
    let scratch = Scratch::new("bad");
    let epic = scratch.create_epic("Epic");

    // Only two children: below the minimum.
    // Child A: low shape labelled high -> tier-mismatch.
    scratch.create_child(&epic, "A", &low_desc("a"), "staged,difficulty:high");
    // Child B: a heading removed -> schema-section.
    let missing = low_desc("b").replace("# Non-goals\n", "");
    scratch.create_child(&epic, "B", &missing, "staged,difficulty:low");

    let out = run_plan_check(&epic, scratch.repo(), &[]);
    assert!(!out.status.success(), "expected a non-zero exit");
    let stdout = stdout_of(&out);
    for rule in ["count-budget", "schema-section", "tier-mismatch"] {
        assert!(stdout.contains(rule), "missing {rule} in:\n{stdout}");
    }
    // Violations are numbered.
    assert!(stdout.contains("1. "), "expected a numbered list:\n{stdout}");
}

#[test]
fn below_minimum_passes_with_a_human_verdict_comment() {
    let scratch = Scratch::new("verdict");
    let epic = scratch.create_epic("Epic");
    for i in 0..3 {
        scratch.create_child(
            &epic,
            &format!("child {i}"),
            &low_desc(&format!("v{i}")),
            "staged,difficulty:low",
        );
    }

    // No verdict: count-budget fails.
    let out = run_plan_check(&epic, scratch.repo(), &[]);
    assert!(!out.status.success());
    assert!(stdout_of(&out).contains("count-budget"));

    // Human verdict comment: the reduced count is accepted.
    scratch.comment(&epic, "verdict: approved as a 3-child pilot");
    let out = run_plan_check(&epic, scratch.repo(), &[]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "OK");
}

#[test]
fn min_width_flag_is_honored() {
    let scratch = Scratch::new("width");
    let epic = scratch.create_epic("Epic");
    let ids: Vec<String> = (0..8)
        .map(|i| {
            scratch.create_child(
                &epic,
                &format!("child {i}"),
                &low_desc(&format!("w{i}")),
                "staged,difficulty:low",
            )
        })
        .collect();
    // Serialize all children behind the first: waves [1, 7] -> median 4.
    for id in &ids[1..] {
        scratch.dep_add(id, &ids[0]);
    }

    // Default min-width 1 passes.
    let out = run_plan_check(&epic, scratch.repo(), &[]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));

    // A min-width above the median fails.
    let out = run_plan_check(&epic, scratch.repo(), &["--min-width", "5"]);
    assert!(!out.status.success());
    assert!(stdout_of(&out).contains("graph-width"));
}
