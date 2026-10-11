//! CP.3 epic sweep + staged decomposition gate end-to-end tests (`agalma-i5h.4`).
//!
//! Each test drives the real [`agalma_conductor::sweep::sweep_epics`] against a
//! scratch `bd` workspace under `target/test-runs/sweep-*` (never `/tmp`):
//! `git init` + `bd init --skip-agents --skip-hooks`. The designer session is a
//! scripted [`DesignerSession`] seam (no harness, no network); the gate and all
//! backlog projections are real.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_contracts::TaskQueueApi;
use agalma_conductor::sweep::{
    sweep_epics, write_live_designer_prompt, DesignerRequest, DesignerSession, PlannerGate,
};
use agalma_taskqueue::BdTaskQueue;
use serde_json::Value;

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
        .join(format!("sweep-{name}-{}-{n}", std::process::id()));
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

/// An isolated git + bd workspace plus a conductor state dir.
struct Scratch {
    repo: PathBuf,
    state_dir: PathBuf,
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
                "sweep",
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
        Scratch { repo, state_dir }
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

    /// Create a deferred, staged child.
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
        let value: Value = serde_json::from_slice(&out.stdout).expect("child json");
        value["id"].as_str().expect("child id").to_string()
    }

    fn create_decomp_fix(&self, parent: &str) -> String {
        let out = self.bd(&[
            "create",
            "decomp-fix: seed",
            "-t",
            "task",
            "-p",
            "1",
            "--parent",
            parent,
            "--description",
            "seed rework task",
            "--labels",
            "decomp-fix",
            "--defer",
            "2099-01-01",
            "--json",
        ]);
        assert!(out.status.success(), "bd create fix: {}", stderr_of(&out));
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

    fn labels(issue: &Value) -> Vec<String> {
        issue["labels"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A child description that satisfies every rule at low tier.
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

/// A scripted designer: records the epics it saw and materializes a fixed shape.
struct ScriptedDesigner {
    mode: Mode,
    called: RefCell<Vec<String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Valid,
    Invalid,
    Noop,
}

impl ScriptedDesigner {
    fn new(mode: Mode) -> Self {
        ScriptedDesigner {
            mode,
            called: RefCell::new(Vec::new()),
        }
    }

    fn called(&self) -> Vec<String> {
        self.called.borrow().clone()
    }
}

impl DesignerSession for ScriptedDesigner {
    fn run(&self, request: &DesignerRequest) -> Result<(), agalma_contracts::ContractError> {
        self.called.borrow_mut().push(request.epic.clone());
        let repo = &request.repo;
        let bd = |args: &[&str]| {
            let out = Command::new("bd")
                .arg("--db")
                .arg(repo.join(".beads"))
                .args(args)
                .current_dir(repo)
                .env("BD_NON_INTERACTIVE", "1")
                .output()
                .expect("spawn bd in designer");
            assert!(out.status.success(), "designer bd {args:?}: {}", stderr_of(&out));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let create = |title: &str, description: &str, labels: &str| {
            let _ = bd(&[
                "create",
                title,
                "-t",
                "task",
                "-p",
                "2",
                "--parent",
                &request.epic,
                "--description",
                description,
                "--design",
                "Small, single-file change; no new interface.",
                "--labels",
                labels,
                "--defer",
                "2099-01-01",
            ]);
        };
        match self.mode {
            Mode::Valid => {
                for i in 0..8 {
                    create(
                        &format!("child {i}"),
                        &low_desc(&format!("c{i}")),
                        "staged,difficulty:low",
                    );
                }
            }
            Mode::Invalid => {
                // A low shape labelled high (tier-mismatch) and a child missing a
                // heading (schema-section); below the minimum too (count-budget).
                create("A", &low_desc("a"), "staged,difficulty:high");
                let missing = low_desc("b").replace("# Non-goals\n", "");
                create("B", &missing, "staged,difficulty:low");
            }
            Mode::Noop => {}
        }
        Ok(())
    }
}

fn gate(allow_paid: bool, paid: bool) -> PlannerGate {
    PlannerGate::new(allow_paid, paid, None)
}

// ---------------------------------------------------------------------------

#[test]
fn undecomposed_epic_is_detected_and_swept() {
    let scratch = Scratch::new("detect");
    let epic = scratch.create_epic("Empty epic");
    let designer = ScriptedDesigner::new(Mode::Noop);

    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(true, true), &designer)
        .expect("sweep");

    assert_eq!(report.epics_seen, vec![epic.clone()], "epic detected");
    assert_eq!(designer.called(), vec![epic], "designer ran for the epic");
}

#[test]
fn valid_decomposition_is_approved_staged_and_invisible_to_ready() {
    let scratch = Scratch::new("approve");
    let epic = scratch.create_epic("Valid epic");
    let designer = ScriptedDesigner::new(Mode::Valid);

    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(true, true), &designer)
        .expect("sweep");
    assert_eq!(report.approved, vec![epic.clone()]);
    assert!(report.rejected.is_empty(), "{report:?}");

    let children = scratch.children(&epic);
    assert_eq!(children.len(), 8, "eight staged children");
    for child in &children {
        let labels = Scratch::labels(child);
        assert!(
            labels.contains(&"decomp-approved".to_string()),
            "child approved: {labels:?}"
        );
        assert_eq!(child["status"], "deferred", "child stays deferred");
    }

    // Ready intake sees neither the epic nor the staged (deferred) children.
    let ready = BdTaskQueue::new(&scratch.repo).ready_tasks().expect("ready");
    assert!(ready.tasks.is_empty(), "no ready tasks: {:?}", ready.tasks);
    assert!(ready.skipped.is_empty(), "nothing skipped: {:?}", ready.skipped);

    // The planner route was resolved and recorded as a `route.model` decision.
    let conn = rusqlite::Connection::open(scratch.state_dir.join("ledger.sqlite"))
        .expect("open ledger");
    let class: String = conn
        .query_row(
            "SELECT json_extract(request_json,'$.context.class') FROM decisions \
             WHERE kind='route.model' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("planner route decision");
    assert_eq!(class, "planner", "route.model class is planner");
}

#[test]
fn invalid_decomposition_is_rejected_with_one_p1_fix_task() {
    let scratch = Scratch::new("reject");
    let epic = scratch.create_epic("Invalid epic");
    let designer = ScriptedDesigner::new(Mode::Invalid);

    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(true, true), &designer)
        .expect("sweep");
    assert_eq!(report.rejected, vec![epic.clone()]);
    assert!(report.escalated.is_empty(), "{report:?}");

    let children = scratch.children(&epic);
    let staged: Vec<&Value> = children
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"staged".to_string()))
        .collect();
    for child in &staged {
        assert!(
            Scratch::labels(child).contains(&"decomp-rejected".to_string()),
            "child rejected: {child}"
        );
    }

    let fixes: Vec<&Value> = children
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"decomp-fix".to_string()))
        .collect();
    assert_eq!(fixes.len(), 1, "exactly one decomp-fix task: {children:?}");
    let fix = fixes[0];
    assert_eq!(fix["priority"], 1, "decomp-fix is P1: {fix}");
    let description = fix["description"].as_str().unwrap_or_default();
    assert!(description.contains("1. count-budget"), "ordered violations: {description}");
    assert!(description.contains("schema-section"), "rule named: {description}");
    assert!(description.contains("tier-mismatch"), "rule named: {description}");

    // The epic carries the rejection comment.
    let epic_issue = scratch.show(&epic);
    let comments: String = epic_issue["comments"]
        .as_array()
        .map(|c| {
            c.iter()
                .filter_map(|v| v["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    assert!(comments.contains("decomp-rejected"), "epic comment: {comments}");
}

#[test]
fn retries_exhausted_escalates_to_human() {
    let scratch = Scratch::new("retries");
    let epic = scratch.create_epic("Stuck epic");

    // Two rejected children and two prior rework tasks: the next failure is the
    // third and exhausts the retry budget.
    scratch.create_child(&epic, "A", &low_desc("a"), "staged,difficulty:high,decomp-rejected");
    scratch.create_child(&epic, "B", &low_desc("b"), "staged,difficulty:low,decomp-rejected");
    for _ in 0..2 {
        scratch.create_decomp_fix(&epic);
    }

    let designer = ScriptedDesigner::new(Mode::Noop);
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(true, true), &designer)
        .expect("sweep");
    assert_eq!(report.escalated, vec![epic.clone()]);

    let epic_issue = scratch.show(&epic);
    assert!(
        Scratch::labels(&epic_issue).contains(&"human".to_string()),
        "epic escalated: {epic_issue}"
    );
    let fixes = scratch
        .children(&epic)
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"decomp-fix".to_string()))
        .count();
    assert_eq!(fixes, 3, "three rework tasks recorded");

    // A human-labelled epic is no longer a candidate.
    let again = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(true, true), &designer)
        .expect("sweep");
    assert!(again.epics_seen.is_empty(), "human epic ignored: {again:?}");
}

#[test]
fn planner_unavailable_skips_without_sessions() {
    let scratch = Scratch::new("unavailable");
    let epic = scratch.create_epic("No planner epic");
    let designer = ScriptedDesigner::new(Mode::Valid);

    // AGALMA_ALLOW_PAID off: the planner route is unavailable.
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(false, true), &designer)
        .expect("sweep");

    assert!(report.skipped_reason.is_some(), "skip recorded: {report:?}");
    assert!(
        report
            .skipped_reason
            .as_deref()
            .unwrap_or_default()
            .contains("never downgrading"),
        "clear reason: {report:?}"
    );
    assert!(designer.called().is_empty(), "no designer session ran");
    assert!(scratch.children(&epic).is_empty(), "no children created");
}

// ---------------------------------------------------------------------------
// Live designer prompt composition (agalma-lve): the per-epic prompt handed to
// the model must name the exact epic and repository and carry retry feedback.
// ---------------------------------------------------------------------------

#[test]
fn live_designer_prompt_carries_epic_repo_and_retry_feedback() {
    let scratch = Scratch::new("designer-prompt");
    let epic = scratch.create_epic("Prompt epic");
    // Seed a rejected child, a rework task, and an epic comment so the retry
    // context is non-empty (the production path gathers these from `bd`).
    let rejected =
        scratch.create_child(&epic, "R", &low_desc("r"), "staged,difficulty:low,decomp-rejected");
    let fix = scratch.create_decomp_fix(&epic);
    scratch.bd_ok(&["comment", &epic, "decomp-rejected: revise the children"]);

    // Materialize the canonical role prompt exactly as `sweep_epics` does.
    let prompts = scratch.state_dir.join("prompts");
    fs::create_dir_all(&prompts).expect("prompts dir");
    let role_path = prompts.join("designer.md");
    fs::write(&role_path, agalma_conductor::DESIGNER_PROMPT).expect("write canonical prompt");

    let request = DesignerRequest {
        epic: epic.clone(),
        repo: scratch.repo.clone(),
        state_dir: scratch.state_dir.clone(),
        model: "planner/model".to_string(),
        prompt_path: role_path,
    };
    let path = write_live_designer_prompt(&request).expect("compose live prompt");
    let text = fs::read_to_string(&path).expect("read composed prompt");

    assert!(text.contains(&format!("epic: {epic}")), "exact epic id: {text}");
    assert!(
        text.contains(&scratch.repo.display().to_string()),
        "owning repository: {text}"
    );
    assert!(text.contains("Prior decomposition feedback"), "{text}");
    assert!(text.contains(&rejected), "rejected child id: {text}");
    assert!(text.contains(&fix), "rework task id: {text}");
    assert!(
        text.contains("decomp-rejected: revise the children"),
        "epic comment feedback: {text}"
    );
    // No misleading placeholder masquerades as the selected id.
    assert!(!text.contains("bd show <epic-id>"), "{text}");
}

// ---------------------------------------------------------------------------
// Human approval release (agalma-4el)
// ---------------------------------------------------------------------------

/// Sorted task ids the adapter's `ready_tasks` admits.
fn ready_ids(repo: &Path) -> Vec<String> {
    let mut ids: Vec<String> = BdTaskQueue::new(repo)
        .ready_tasks()
        .expect("ready")
        .tasks
        .iter()
        .map(|t| t.task_id.as_str().to_string())
        .collect();
    ids.sort();
    ids
}

/// End-to-end: a clean decomposition passes the gate; children stay invisible;
/// a single human `bd undefer` releases one approved child when it is unblocked,
/// while every other deferred child stays hidden.
#[test]
fn approved_children_release_only_after_human_undefer_and_unblock() {
    let scratch = Scratch::new("undefer-release");
    let epic = scratch.create_epic("Valid epic");
    let designer = ScriptedDesigner::new(Mode::Valid);
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(true, true), &designer)
        .expect("sweep");
    assert_eq!(report.approved, vec![epic.clone()], "{report:?}");

    let ids: Vec<String> = scratch
        .children(&epic)
        .iter()
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(ids.len(), 8, "eight approved children");

    // Validator-approved but not human-authorized: nothing is ready.
    assert!(
        ready_ids(&scratch.repo).is_empty(),
        "approved children stay deferred until `bd undefer`"
    );

    // Serialize child 1 behind child 0, then authorize both with `bd undefer`.
    scratch.bd_ok(&["dep", "add", &ids[1], &ids[0]]);
    scratch.bd_ok(&["undefer", &ids[0]]);
    scratch.bd_ok(&["undefer", &ids[1]]);

    // Only the unblocked child is ready; the blocked one is withheld.
    assert_eq!(
        ready_ids(&scratch.repo),
        vec![ids[0].clone()],
        "undeferred approved child ready; blocked one withheld"
    );

    // Closing the blocker releases the blocked approved child.
    scratch.bd_ok(&["close", &ids[0], "--reason", "done in test"]);
    assert_eq!(
        ready_ids(&scratch.repo),
        vec![ids[1].clone()],
        "blocked approved child releases once its prerequisite closes"
    );

    // The remaining six children were never undeferred and stay invisible.
    let ready = ready_ids(&scratch.repo);
    for never_undeferred in &ids[2..] {
        assert!(
            !ready.contains(never_undeferred),
            "deferred child {never_undeferred} leaked: {ready:?}"
        );
    }
}

/// Rejected and never-validated staged children never become executable — not
/// by `bd undefer`, and not by an accidental status change.
#[test]
fn rejected_and_unvalidated_children_stay_hidden() {
    let scratch = Scratch::new("rejected-hidden");
    let epic = scratch.create_epic("Invalid epic");
    let designer = ScriptedDesigner::new(Mode::Invalid);
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(true, true), &designer)
        .expect("sweep");
    assert_eq!(report.rejected, vec![epic.clone()], "{report:?}");

    let rejected: Vec<String> = scratch
        .children(&epic)
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"decomp-rejected".to_string()))
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(rejected.len(), 2, "both staged children rejected");

    // Human `undefer` of a rejected child does not release it.
    scratch.bd_ok(&["undefer", &rejected[0]]);
    assert!(
        ready_ids(&scratch.repo).is_empty(),
        "a rejected child stays hidden after `bd undefer`"
    );

    // A never-validated staged child cannot be released by an accidental status
    // change either.
    let unvalidated = scratch.create_child(&epic, "U", &low_desc("u"), "staged,difficulty:low");
    scratch.bd_ok(&["update", &unvalidated, "--status", "open"]);
    assert!(
        ready_ids(&scratch.repo).is_empty(),
        "a never-validated staged child stays hidden after a status change"
    );
}
