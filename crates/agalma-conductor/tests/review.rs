//! CP.5 per-epic review gate end-to-end tests (`agalma-i5h.6`).
//!
//! Each test drives the real [`agalma_conductor::review`] gate against a scratch
//! `git` + `bd` workspace under `target/test-runs/review-*` (never `/tmp`): a
//! real repository with real commits, so the diff baseline and head are real,
//! and a real isolated `bd` store. The reviewer session is a scripted
//! [`ReviewerSession`] seam (no harness, no network, no model); the marker
//! parsing, drift-fix filing, dispatch hold, and retry escalation are all real.
//! Scratch dirs auto-remove on success (kept on failure for forensics).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_conductor::review::{
    dispatch_hold, parse_verdict, record_start_on_land, review_epics, review_start_sha,
    ReviewRequest, ReviewVerdict, ReviewerSession, MAX_REVIEW_RETRIES, REVIEW_DRIFT_LABEL,
    REVIEW_START_PREFIX,
};
use agalma_conductor::sweep::PlannerGate;
use agalma_taskqueue::BdTaskQueue;
use agalma_testkit::ScratchDir;
use serde_json::Value;

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(&format!("review-{name}"))
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

/// An isolated git + bd workspace plus a conductor state dir.
struct Scratch {
    repo: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Scratch {
    fn new(name: &str) -> Self {
        require_bd();
        let dir = fresh(name);
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).expect("repo dir");
        run_ok("git", &repo, &["init", "-q"]);
        run_ok("git", &repo, &["config", "user.email", "agalma@localhost"]);
        run_ok("git", &repo, &["config", "user.name", "Agalma"]);

        let out = Command::new("bd")
            .args([
                "init",
                "--prefix",
                "rev",
                "--non-interactive",
                "--skip-agents",
                "--skip-hooks",
            ])
            .current_dir(&repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd init");
        assert!(out.status.success(), "bd init: {}", stderr_of(&out));

        let state_dir = dir.join("state");
        fs::create_dir_all(&state_dir).expect("state dir");
        Scratch {
            repo,
            state_dir,
            _dir: dir,
        }
    }

    fn beads(&self) -> PathBuf {
        self.repo.join(".beads")
    }

    fn bd(&self, args: &[&str]) -> std::process::Output {
        Command::new("bd")
            .arg("--db")
            .arg(self.beads())
            .args(args)
            .current_dir(&self.repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd")
    }

    fn bd_ok(&self, args: &[&str]) -> String {
        let out = self.bd(args);
        assert!(out.status.success(), "bd {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "git {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Commit an (empty) change and return the new HEAD sha.
    fn commit(&self, message: &str) -> String {
        run_ok(
            "git",
            &self.repo,
            &["commit", "--allow-empty", "-q", "-m", message],
        );
        self.git(&["rev-parse", "HEAD"])
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
            "epic design",
            "--json",
        ]);
        assert!(out.status.success(), "bd create epic: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("epic json");
        value["id"].as_str().expect("epic id").to_string()
    }

    /// Create a staged child (`staged` + a valid low shape) and close it, so the
    /// epic's decomposition has fully landed.
    fn create_closed_child(&self, parent: &str, title: &str) -> String {
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
            &low_desc(title),
            "--labels",
            "staged,difficulty:low",
            "--json",
        ]);
        assert!(out.status.success(), "bd create child: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("child json");
        let id = value["id"].as_str().expect("child id").to_string();
        self.bd_ok(&["close", &id, "--reason", "landed"]);
        id
    }

    fn create_drift_fix(&self, parent: &str) -> String {
        let out = self.bd(&[
            "create",
            "drift-fix: seed",
            "-t",
            "task",
            "-p",
            "1",
            "--parent",
            parent,
            "--description",
            "seed drift rework",
            "--labels",
            "drift-fix",
            "--json",
        ]);
        assert!(
            out.status.success(),
            "bd create drift-fix: {}",
            stderr_of(&out)
        );
        let value: Value = serde_json::from_slice(&out.stdout).expect("fix json");
        value["id"].as_str().expect("fix id").to_string()
    }

    fn show(&self, id: &str) -> Value {
        let stdout = self.bd_ok(&["show", id, "--json", "--include-comments"]);
        let value: Value = serde_json::from_str(&stdout).expect("show json");
        value
            .as_array()
            .expect("show array")
            .first()
            .expect("one issue")
            .clone()
    }

    fn children(&self, epic: &str) -> Vec<Value> {
        let stdout = self.bd_ok(&["list", "--parent", epic, "--all", "--json", "--limit", "0"]);
        serde_json::from_str(&stdout).expect("children json")
    }

    fn labels(value: &Value) -> Vec<String> {
        value["labels"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn epic_labels(&self, epic: &str) -> Vec<String> {
        Scratch::labels(&self.show(epic))
    }
}

/// A minimal valid low-tier child description.
fn low_desc(name: &str) -> String {
    format!(
        "# Scope\nDo exactly one small thing for {name}.\n\n\
         # Files\ncrates/foo/src/{name}.rs\n\n\
         # Contract\nRun the acceptance command.\n\n\
         # Invariants\nNo behavior change outside scope.\n\n\
         # Non-goals\nNo refactor.\n\n\
         # MACHINE\nprovides:\nconsumes:\nfiles: crates/foo/src/{name}.rs\ntier: low\n"
    )
}

/// A scripted reviewer that returns a fixed marker string.
struct ScriptedReviewer {
    output: String,
}

impl ScriptedReviewer {
    fn new(output: &str) -> Self {
        ScriptedReviewer {
            output: output.to_string(),
        }
    }
}

impl ReviewerSession for ScriptedReviewer {
    fn run(&self, _request: &ReviewRequest) -> Result<String, agalma_contracts::ContractError> {
        Ok(self.output.clone())
    }
}

fn gate() -> PlannerGate {
    PlannerGate::new(true, true, None)
}

/// Build an epic with one closed staged child and a recorded review baseline,
/// returning `(epic, baseline_sha)`.
fn landed_epic(scratch: &Scratch, name: &str) -> (String, String) {
    let base = scratch.commit("base");
    let epic = scratch.create_epic(name);
    scratch.create_closed_child(&epic, "child 0");
    // The head advances past the baseline so the review diff is non-empty.
    scratch.commit("work");
    scratch.bd_ok(&[
        "update",
        &epic,
        "--add-label",
        &format!("{REVIEW_START_PREFIX}{base}"),
    ]);
    (epic, base)
}

#[test]
fn drift_verdict_files_one_p1_drift_fix_and_holds_dispatch() {
    let scratch = Scratch::new("drift");
    let (epic, base) = landed_epic(&scratch, "Drifting epic");
    let reviewer = ScriptedReviewer::new("reasoning...\nREVIEW: DRIFT missing acceptance for X\n");

    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(report.epics_seen, vec![epic.clone()], "{report:?}");
    assert_eq!(report.drift, vec![epic.clone()], "{report:?}");
    assert!(report.approved.is_empty(), "{report:?}");

    // Epic carries the review baseline and the active hold.
    let labels = scratch.epic_labels(&epic);
    assert_eq!(
        review_start_sha(&labels).as_deref(),
        Some(base.as_str()),
        "baseline recorded: {labels:?}"
    );
    assert!(
        labels.contains(&REVIEW_DRIFT_LABEL.to_string()),
        "{labels:?}"
    );
    assert_eq!(
        dispatch_hold(&BdTaskQueue::new(&scratch.repo)).expect("hold"),
        Some(epic.clone()),
        "ordinary dispatch is held"
    );

    // Exactly one P1 drift-fix task, excluded from `bd ready` by its
    // `drift-fix` label (never a sentinel defer date), carrying the reviewer
    // feedback.
    let children = scratch.children(&epic);
    let fixes: Vec<&Value> = children
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"drift-fix".to_string()))
        .collect();
    assert_eq!(fixes.len(), 1, "exactly one drift-fix: {fixes:?}");
    let fix = fixes[0];
    assert_eq!(fix["priority"], 1, "drift-fix is P1: {fix}");
    assert_eq!(fix["status"], "open", "drift-fix is rework: {fix}");
    assert!(
        fix.get("defer_until").is_none() || fix["defer_until"].is_null(),
        "rework is label-excluded, not defer-hidden: {fix}"
    );
    let description = fix["description"].as_str().unwrap_or_default();
    assert!(
        description.contains("missing acceptance for X"),
        "{description}"
    );
    assert!(description.contains(&base), "baseline named: {description}");
}

#[test]
fn approved_verdict_clears_the_epic_and_releases_the_hold() {
    let scratch = Scratch::new("approved");
    let (epic, _base) = landed_epic(&scratch, "Clean epic");
    let reviewer = ScriptedReviewer::new("looks complete\nREVIEW: APPROVED\n");

    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(report.approved, vec![epic.clone()], "{report:?}");
    assert!(report.drift.is_empty(), "{report:?}");

    let labels = scratch.epic_labels(&epic);
    assert!(
        labels.contains(&"review-approved".to_string()),
        "{labels:?}"
    );
    assert!(
        !labels.contains(&REVIEW_DRIFT_LABEL.to_string()),
        "{labels:?}"
    );
    assert_eq!(
        dispatch_hold(&BdTaskQueue::new(&scratch.repo)).expect("hold"),
        None,
        "no hold after approval"
    );
    // No rework task was filed.
    let fixes = scratch
        .children(&epic)
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"drift-fix".to_string()))
        .count();
    assert_eq!(fixes, 0, "no drift-fix on approval");

    // An approved epic is terminal: a second pass does not re-review it.
    let again =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");
    assert!(
        again.epics_seen.is_empty(),
        "approved epic not re-reviewed: {again:?}"
    );
}

#[test]
fn missing_marker_errors_and_never_silently_approves() {
    let scratch = Scratch::new("no-marker");
    let (epic, _base) = landed_epic(&scratch, "Unmarked epic");
    let reviewer = ScriptedReviewer::new("I think this is fine but emitted no marker\n");

    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(report.errored, vec![epic.clone()], "{report:?}");
    assert!(
        report.approved.is_empty(),
        "never a silent pass: {report:?}"
    );
    let labels = scratch.epic_labels(&epic);
    assert!(
        !labels.contains(&"review-approved".to_string()),
        "must not approve: {labels:?}"
    );
    assert!(
        labels.contains(&REVIEW_DRIFT_LABEL.to_string()),
        "held: {labels:?}"
    );
    assert_eq!(
        dispatch_hold(&BdTaskQueue::new(&scratch.repo)).expect("hold"),
        Some(epic.clone()),
        "an errored review holds dispatch (fail closed)"
    );
    // No rework task is fabricated for a mere error.
    let fixes = scratch
        .children(&epic)
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"drift-fix".to_string()))
        .count();
    assert_eq!(fixes, 0, "no drift-fix on a marker error");
}

#[test]
fn repeated_drift_exhausts_the_budget_and_escalates_to_a_human() {
    let scratch = Scratch::new("retries");
    let (epic, _base) = landed_epic(&scratch, "Stubborn epic");

    // Two prior drift attempts already recorded (two drift-fix tasks + counter):
    // this pass is the third and exhausts the budget.
    scratch.create_drift_fix(&epic);
    scratch.create_drift_fix(&epic);
    scratch.bd_ok(&["update", &epic, "--add-label", "review-attempts:2"]);

    let reviewer = ScriptedReviewer::new("REVIEW: DRIFT still wrong");
    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(report.drift, vec![epic.clone()], "{report:?}");
    assert_eq!(report.escalated, vec![epic.clone()], "{report:?}");
    assert_eq!(MAX_REVIEW_RETRIES, 3, "budget documented as three");

    let labels = scratch.epic_labels(&epic);
    assert!(
        labels.contains(&"human".to_string()),
        "escalated: {labels:?}"
    );
    assert!(
        labels.contains(&"review-attempts:3".to_string()),
        "{labels:?}"
    );

    let fixes = scratch
        .children(&epic)
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"drift-fix".to_string()))
        .count();
    assert_eq!(fixes, 3, "three attempts recorded");

    // An escalated epic no longer holds ordinary dispatch (a human owns it).
    assert_eq!(
        dispatch_hold(&BdTaskQueue::new(&scratch.repo)).expect("hold"),
        None,
        "human escalation lifts the hold"
    );

    // It is also never re-reviewed.
    let again =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");
    assert!(
        again.epics_seen.is_empty(),
        "escalated epic ignored: {again:?}"
    );
}

#[test]
fn record_start_on_land_records_the_head_once() {
    let scratch = Scratch::new("start-sha");
    let epic = scratch.create_epic("Fresh epic");
    let child = scratch.create_closed_child(&epic, "child 0");

    let first = scratch.commit("base");
    record_start_on_land(&scratch.repo, &child).expect("record baseline");
    let labels = scratch.epic_labels(&epic);
    assert_eq!(review_start_sha(&labels).as_deref(), Some(first.as_str()));

    // A later land must not move an existing baseline (first land wins).
    let _second = scratch.commit("more");
    record_start_on_land(&scratch.repo, &child).expect("second record is a no-op");
    let labels = scratch.epic_labels(&epic);
    assert_eq!(review_start_sha(&labels).as_deref(), Some(first.as_str()));
}

#[test]
fn parse_verdict_is_strict_about_the_marker() {
    assert_eq!(
        parse_verdict("REVIEW: APPROVED").unwrap(),
        ReviewVerdict::Approved
    );
    assert!(matches!(
        parse_verdict("REVIEW: DRIFT x").unwrap(),
        ReviewVerdict::Drift { .. }
    ));
    assert!(parse_verdict("no marker").is_err(), "absent → error");
    assert!(
        parse_verdict("REVIEW: APPROVED\nREVIEW: DRIFT x").is_err(),
        "two markers → error"
    );
}
