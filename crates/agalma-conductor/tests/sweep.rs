//! CP.3 epic sweep + staged decomposition gate end-to-end tests (`agalma-i5h.4`).
//!
//! Each test drives the real [`agalma_conductor::sweep::sweep_epics`] against a
//! scratch `bd` workspace under `target/test-runs/sweep-*` (never `/tmp`):
//! `git init` + `bd init --skip-agents --skip-hooks`. The designer session is a
//! scripted [`DesignerSession`] seam (no harness, no network); the gate and all
//! backlog projections are real. Scratch dirs auto-remove on success (kept on
//! failure for forensics).

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_conductor::sweep::{
    sweep_epic, sweep_epics, write_live_designer_prompt, DesignerOutcome, DesignerRequest,
    DesignerSession, PlannerGate,
};
use agalma_contracts::TaskQueueApi;
use agalma_taskqueue::BdTaskQueue;
use agalma_testkit::ScratchDir;
use serde_json::Value;

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(&format!("sweep-{name}"))
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
    /// Whether a `FailAfterOne` designer has already emitted its partial child.
    emitted_partial: RefCell<bool>,
    /// Number of `TimeoutAfterPartial` runs completed (drives first-run vs retry).
    runs: RefCell<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Valid,
    Invalid,
    Noop,
    /// Create exactly one staged child on the first run, then fail the session
    /// (an interrupted designer that dies mid-decomposition).
    FailAfterOne,
    /// Commit a complete valid decomposition, then hit the turn bound.
    TimeoutAfterValid,
    /// Commit two valid staged children on the first run then hit the bound;
    /// commit six more (a complete eight) on the retry.
    TimeoutAfterPartial,
    /// Hit the turn bound without committing anything.
    TimeoutNoop,
}

impl ScriptedDesigner {
    fn new(mode: Mode) -> Self {
        ScriptedDesigner {
            mode,
            called: RefCell::new(Vec::new()),
            emitted_partial: RefCell::new(false),
            runs: RefCell::new(0),
        }
    }

    fn called(&self) -> Vec<String> {
        self.called.borrow().clone()
    }
}

impl DesignerSession for ScriptedDesigner {
    fn run(
        &self,
        request: &DesignerRequest,
    ) -> Result<DesignerOutcome, agalma_contracts::ContractError> {
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
            assert!(
                out.status.success(),
                "designer bd {args:?}: {}",
                stderr_of(&out)
            );
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
        let outcome = match self.mode {
            Mode::Valid => {
                for i in 0..8 {
                    create(
                        &format!("child {i}"),
                        &low_desc(&format!("c{i}")),
                        "staged,difficulty:low",
                    );
                }
                DesignerOutcome::Completed
            }
            Mode::Invalid => {
                // A low shape labelled high (tier-mismatch) and a child missing a
                // heading (schema-section); below the minimum too (count-budget).
                create("A", &low_desc("a"), "staged,difficulty:high");
                let missing = low_desc("b").replace("# Non-goals\n", "");
                create("B", &missing, "staged,difficulty:low");
                DesignerOutcome::Completed
            }
            Mode::Noop => DesignerOutcome::Completed,
            Mode::FailAfterOne => {
                // The first run leaves one staged child behind and then dies;
                // later runs fail without creating anything new, so any
                // duplicate child in the backlog would be the conductor's doing.
                let mut emitted = self.emitted_partial.borrow_mut();
                if !*emitted {
                    create("partial", &low_desc("partial"), "staged,difficulty:low");
                    *emitted = true;
                }
                return Err(agalma_contracts::ContractError::KnownFailure(
                    "scripted designer failure after one child".to_string(),
                ));
            }
            Mode::TimeoutAfterValid => {
                // Commit a complete valid decomposition, then cross the bound.
                for i in 0..8 {
                    create(
                        &format!("child {i}"),
                        &low_desc(&format!("c{i}")),
                        "staged,difficulty:low",
                    );
                }
                DesignerOutcome::TimedOut
            }
            Mode::TimeoutAfterPartial => {
                let mut runs = self.runs.borrow_mut();
                *runs += 1;
                let run = *runs;
                if run == 1 {
                    // Commit two valid staged children then hit the bound.
                    for i in 0..2 {
                        create(
                            &format!("child {i}"),
                            &low_desc(&format!("c{i}")),
                            "staged,difficulty:low",
                        );
                    }
                    DesignerOutcome::TimedOut
                } else {
                    // Retry: complete the decomposition (six more children).
                    for i in 2..8 {
                        create(
                            &format!("child {i}"),
                            &low_desc(&format!("c{i}")),
                            "staged,difficulty:low",
                        );
                    }
                    DesignerOutcome::Completed
                }
            }
            Mode::TimeoutNoop => DesignerOutcome::TimedOut,
        };
        Ok(outcome)
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

    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");

    assert_eq!(report.epics_seen, vec![epic.clone()], "epic detected");
    assert_eq!(designer.called(), vec![epic], "designer ran for the epic");
}

#[test]
fn valid_decomposition_is_approved_staged_and_invisible_to_ready() {
    let scratch = Scratch::new("approve");
    let epic = scratch.create_epic("Valid epic");
    let designer = ScriptedDesigner::new(Mode::Valid);

    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
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
    let ready = BdTaskQueue::new(&scratch.repo)
        .ready_tasks()
        .expect("ready");
    assert!(ready.tasks.is_empty(), "no ready tasks: {:?}", ready.tasks);
    assert!(
        ready.skipped.is_empty(),
        "nothing skipped: {:?}",
        ready.skipped
    );

    // The planner route was resolved and recorded as a `route.model` decision.
    let conn =
        rusqlite::Connection::open(scratch.state_dir.join("ledger.sqlite")).expect("open ledger");
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

    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
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
    assert!(
        description.contains("1. count-budget"),
        "ordered violations: {description}"
    );
    assert!(
        description.contains("schema-section"),
        "rule named: {description}"
    );
    assert!(
        description.contains("tier-mismatch"),
        "rule named: {description}"
    );

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
    assert!(
        comments.contains("decomp-rejected"),
        "epic comment: {comments}"
    );
}

#[test]
fn retries_exhausted_escalates_to_human() {
    let scratch = Scratch::new("retries");
    let epic = scratch.create_epic("Stuck epic");

    // Two rejected children and two prior rework tasks: the next failure is the
    // third and exhausts the retry budget.
    scratch.create_child(
        &epic,
        "A",
        &low_desc("a"),
        "staged,difficulty:high,decomp-rejected",
    );
    scratch.create_child(
        &epic,
        "B",
        &low_desc("b"),
        "staged,difficulty:low,decomp-rejected",
    );
    for _ in 0..2 {
        scratch.create_decomp_fix(&epic);
    }

    let designer = ScriptedDesigner::new(Mode::Noop);
    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
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
    let again = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert!(again.epics_seen.is_empty(), "human epic ignored: {again:?}");
}

#[test]
fn planner_unavailable_skips_without_sessions() {
    let scratch = Scratch::new("unavailable");
    let epic = scratch.create_epic("No planner epic");
    let designer = ScriptedDesigner::new(Mode::Valid);

    // AGALMA_ALLOW_PAID off: the planner route is unavailable.
    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(false, true),
        &designer,
    )
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
    let rejected = scratch.create_child(
        &epic,
        "R",
        &low_desc("r"),
        "staged,difficulty:low,decomp-rejected",
    );
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

    assert!(
        text.contains(&format!("epic: {epic}")),
        "exact epic id: {text}"
    );
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
    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
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
    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
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

// ---------------------------------------------------------------------------
// Durable per-epic planning outcome (agalma-yy1): interrupted, empty, and
// partially projected decompositions retry across restart, preserve child
// identity, and escalate at the retry bound; approved epics stay untouched.
// ---------------------------------------------------------------------------

/// The number of `decomp-fix` rework tasks parented to `epic`.
fn fix_count(scratch: &Scratch, epic: &str) -> usize {
    scratch
        .children(epic)
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"decomp-fix".to_string()))
        .count()
}

/// The staged (decomposition membership) child ids under `epic`, sorted.
fn staged_ids(scratch: &Scratch, epic: &str) -> Vec<String> {
    let mut ids: Vec<String> = scratch
        .children(epic)
        .iter()
        .filter(|c| Scratch::labels(c).contains(&"staged".to_string()))
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect();
    ids.sort();
    ids
}

fn has_label(value: &Value, label: &str) -> bool {
    Scratch::labels(value).contains(&label.to_string())
}

#[test]
fn designer_failure_after_one_child_retries_and_preserves_child_identity() {
    let scratch = Scratch::new("fail-after-one");
    let epic = scratch.create_epic("Partial epic");
    let designer = ScriptedDesigner::new(Mode::FailAfterOne);

    // Sweep 1: the session dies after creating one child. The conductor must
    // reconcile the orphan (reject it, record the attempt) instead of stranding
    // the epic outside retry handling.
    let first = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(first.epics_seen, vec![epic.clone()]);
    assert_eq!(first.rejected, vec![epic.clone()], "{first:?}");
    assert!(first.escalated.is_empty(), "{first:?}");

    let orphans = staged_ids(&scratch, &epic);
    assert_eq!(orphans.len(), 1, "one partial child: {orphans:?}");
    let orphan = orphans[0].clone();
    assert!(
        has_label(&scratch.show(&orphan), "decomp-rejected"),
        "the orphan is reconciled (rejected), not forgotten"
    );
    assert_eq!(fix_count(&scratch, &epic), 1, "attempt 1 recorded");

    // Sweep 2: the same epic is retried; the orphan keeps its identity and no
    // duplicate decomposition child appears.
    let second = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(second.epics_seen, vec![epic.clone()], "epic retried");
    assert_eq!(designer.called().len(), 2, "designer ran again");
    assert_eq!(
        staged_ids(&scratch, &epic),
        vec![orphan.clone()],
        "child identity preserved, no duplicate"
    );
    assert_eq!(fix_count(&scratch, &epic), 2, "attempt 2 recorded");

    // Sweep 3: the retry bound is reached and the epic escalates to a human.
    let third = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(third.escalated, vec![epic.clone()], "{third:?}");
    assert_eq!(fix_count(&scratch, &epic), 3, "three attempts recorded");
    assert!(
        has_label(&scratch.show(&epic), "human"),
        "epic escalated to a human"
    );

    // Sweep 4: an escalated epic is never re-dispatched.
    let fourth = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert!(
        fourth.epics_seen.is_empty(),
        "human epic ignored: {fourth:?}"
    );
    assert_eq!(designer.called().len(), 3, "no further designer session");
}

#[test]
fn success_with_zero_children_retries_and_escalates() {
    let scratch = Scratch::new("zero-children");
    let epic = scratch.create_epic("Empty epic");
    let designer = ScriptedDesigner::new(Mode::Noop);

    // Sweep 1: a completed session that produced no children is a failed
    // decomposition, not a dead end.
    let first = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(first.epics_seen, vec![epic.clone()]);
    assert_eq!(first.rejected, vec![epic.clone()], "{first:?}");
    assert!(first.escalated.is_empty(), "{first:?}");
    assert_eq!(fix_count(&scratch, &epic), 1, "empty attempt recorded");

    // Sweep 2: the empty epic is still a candidate and retried.
    let second = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(second.epics_seen, vec![epic.clone()], "empty epic retried");
    assert_eq!(designer.called().len(), 2, "designer ran again");
    assert_eq!(fix_count(&scratch, &epic), 2);

    // Sweep 3: the retry bound is reached and the epic escalates.
    let third = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(third.escalated, vec![epic.clone()], "{third:?}");
    assert_eq!(fix_count(&scratch, &epic), 3);
    assert!(has_label(&scratch.show(&epic), "human"), "escalated");

    // Sweep 4: escalated epics are not re-dispatched.
    let fourth = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert!(fourth.epics_seen.is_empty(), "{fourth:?}");
}

#[test]
fn restart_after_partial_gate_projection_recovers_without_replanning() {
    let scratch = Scratch::new("restart-partial");
    let epic = scratch.create_epic("Interrupted epic");

    // Simulate a process death mid-gate: a complete valid decomposition whose
    // approval was projected for only three of eight children, with the durable
    // in-flight marker and attempt counter still present.
    let ids: Vec<String> = (0..8)
        .map(|i| {
            scratch.create_child(
                &epic,
                &format!("child {i}"),
                &low_desc(&format!("c{i}")),
                "staged,difficulty:low",
            )
        })
        .collect();
    for id in &ids[..3] {
        scratch.bd_ok(&["update", id, "--add-label", "decomp-approved"]);
    }
    // A stale rejection from an even earlier attempt: a passing gate must clear
    // it so a repaired decomposition can recover.
    scratch.bd_ok(&["update", &ids[3], "--add-label", "decomp-rejected"]);
    scratch.bd_ok(&["update", &epic, "--add-label", "decomp-attempts:1"]);
    scratch.bd_ok(&["update", &epic, "--add-label", "decomp-inflight"]);

    let designer = ScriptedDesigner::new(Mode::Valid);
    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");

    // The interrupted attempt is finalized in place: no new designer session,
    // no duplicated children, and the repaired decomposition passes the gate.
    assert_eq!(report.recovered, vec![epic.clone()], "{report:?}");
    assert!(report.epics_swept.is_empty(), "no new session: {report:?}");
    assert!(designer.called().is_empty(), "designer never re-run");
    assert_eq!(report.approved, vec![epic.clone()], "{report:?}");

    assert_eq!(staged_ids(&scratch, &epic), {
        let mut sorted = ids.clone();
        sorted.sort();
        sorted
    });
    for id in &ids {
        let issue = scratch.show(id);
        assert!(
            has_label(&issue, "decomp-approved"),
            "child {id} approved by recovery"
        );
        assert!(
            !has_label(&issue, "decomp-rejected"),
            "child {id} stale rejection cleared by the passing gate"
        );
    }
    assert!(
        !has_label(&scratch.show(&epic), "decomp-inflight"),
        "in-flight marker cleared"
    );

    // Recovered-but-approved children stay invisible until a human `bd undefer`.
    assert!(
        ready_ids(&scratch.repo).is_empty(),
        "approved children stay deferred pending `bd undefer`"
    );

    // A later sweep sees the approved epic as untouched.
    let again = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert!(again.epics_seen.is_empty(), "{again:?}");
}

#[test]
fn hand_authored_epic_children_are_not_replanned() {
    let scratch = Scratch::new("hand-authored");
    let epic = scratch.create_epic("Hand-authored epic");
    // A child with no `staged` marker is not the sweep's decomposition to touch.
    scratch.create_child(&epic, "hand", &low_desc("h"), "difficulty:low");

    let designer = ScriptedDesigner::new(Mode::Valid);
    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");

    assert!(report.epics_seen.is_empty(), "{report:?}");
    assert!(designer.called().is_empty(), "designer never ran");
    assert_eq!(scratch.children(&epic).len(), 1, "children untouched");
}

#[test]
fn approved_epic_is_not_redispatched() {
    let scratch = Scratch::new("approved-untouched");
    let epic = scratch.create_epic("Valid epic");
    let designer = ScriptedDesigner::new(Mode::Valid);

    let first = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(first.approved, vec![epic.clone()], "{first:?}");

    let second = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert!(second.epics_seen.is_empty(), "{second:?}");
    assert_eq!(
        designer.called().len(),
        1,
        "no re-dispatch of an approved epic"
    );
}

// ---------------------------------------------------------------------------
// Timeout after commit (agalma-cmq): a designer that hits the wall bound after
// committing children must be reconciled by the gate in the same pass — never a
// second paid session for the same attempt, never a deferred re-sweep.
// ---------------------------------------------------------------------------

#[test]
fn designer_timeout_after_full_commit_finalizes_in_place_without_second_session() {
    let scratch = Scratch::new("timeout-full");
    let epic = scratch.create_epic("Timed-out full epic");
    let designer = ScriptedDesigner::new(Mode::TimeoutAfterValid);

    // Sweep 1: the designer commits a complete valid decomposition, then hits
    // the turn bound. The same pass reconciles it: approved in place, no second
    // session, and the timeout is recorded.
    let first = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(first.timed_out, vec![epic.clone()], "{first:?}");
    assert_eq!(first.approved, vec![epic.clone()], "{first:?}");
    assert!(
        first.epics_swept.is_empty(),
        "a timed-out session is not 'swept' as completed: {first:?}"
    );
    assert_eq!(designer.called().len(), 1, "exactly one designer session");

    let children = scratch.children(&epic);
    assert_eq!(children.len(), 8, "eight staged children: {children:?}");
    for child in &children {
        let labels = Scratch::labels(child);
        assert!(
            labels.contains(&"decomp-approved".to_string()),
            "child approved by same-pass reconciliation: {labels:?}"
        );
        assert_eq!(child["status"], "deferred", "child stays deferred");
    }
    let epic_issue = scratch.show(&epic);
    assert!(
        !has_label(&epic_issue, "decomp-inflight"),
        "in-flight marker cleared: {epic_issue}"
    );
    let comments: String = epic_issue["comments"]
        .as_array()
        .map(|c| {
            c.iter()
                .filter_map(|v| v["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    assert!(
        comments.contains("decomp-approved"),
        "epic carries the approval comment: {comments}"
    );
    assert!(
        ready_ids(&scratch.repo).is_empty(),
        "approved children stay deferred pending `bd undefer`"
    );

    // Sweep 2: the approved epic is untouched and no further session runs.
    let second = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert!(second.epics_seen.is_empty(), "{second:?}");
    assert_eq!(designer.called().len(), 1, "no second paid session");
}

#[test]
fn designer_timeout_after_partial_commit_rejects_then_retry_completes() {
    let scratch = Scratch::new("timeout-partial");
    let epic = scratch.create_epic("Timed-out partial epic");
    let designer = ScriptedDesigner::new(Mode::TimeoutAfterPartial);

    // Sweep 1: two valid children committed, then the bound. The incomplete set
    // fails the gate and is rejected; the attempt is recorded and the in-flight
    // marker cleared.
    let first = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(first.timed_out, vec![epic.clone()], "{first:?}");
    assert_eq!(first.rejected, vec![epic.clone()], "{first:?}");
    assert_eq!(fix_count(&scratch, &epic), 1, "attempt 1 recorded");
    let original = staged_ids(&scratch, &epic);
    assert_eq!(original.len(), 2, "two partial children: {original:?}");
    for id in &original {
        assert!(
            has_label(&scratch.show(id), "decomp-rejected"),
            "partial child rejected: {id}"
        );
    }
    assert!(
        !has_label(&scratch.show(&epic), "decomp-inflight"),
        "in-flight marker cleared after reconciliation"
    );

    // Sweep 2: the retry completes the decomposition. The original children keep
    // their identity (no duplicates), the stale rejections are cleared, and the
    // repaired set is approved.
    let second = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(designer.called().len(), 2, "retry ran the designer again");
    assert_eq!(second.approved, vec![epic.clone()], "{second:?}");

    let staged = staged_ids(&scratch, &epic);
    assert_eq!(staged.len(), 8, "eight staged children after retry: {staged:?}");
    for id in &original {
        assert_eq!(
            staged.iter().filter(|s| *s == id).count(),
            1,
            "original child {id} present exactly once: {staged:?}"
        );
    }
    for id in &staged {
        let issue = scratch.show(id);
        assert!(
            has_label(&issue, "decomp-approved"),
            "child {id} approved by the passing gate"
        );
        assert!(
            !has_label(&issue, "decomp-rejected"),
            "child {id} stale rejection cleared"
        );
    }

    // Sweep 3: the approved epic is untouched.
    let third = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert!(third.epics_seen.is_empty(), "{third:?}");
}

#[test]
fn designer_timeout_with_no_commits_counts_toward_retry_budget() {
    let scratch = Scratch::new("timeout-noop");
    let epic = scratch.create_epic("Timed-out empty epic");
    let designer = ScriptedDesigner::new(Mode::TimeoutNoop);

    // Sweeps 1 and 2: a timeout with no committed children is a failed
    // decomposition, retried like any other attempt.
    for expected in 1..=2 {
        let report = sweep_epics(
            &scratch.repo,
            &scratch.state_dir,
            &gate(true, true),
            &designer,
        )
        .expect("sweep");
        assert_eq!(report.timed_out, vec![epic.clone()], "{report:?}");
        assert_eq!(report.rejected, vec![epic.clone()], "{report:?}");
        assert!(report.escalated.is_empty(), "{report:?}");
        assert_eq!(fix_count(&scratch, &epic), expected, "attempt recorded");
    }

    // Sweep 3: the retry bound is reached and the epic escalates to a human.
    let third = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert_eq!(third.escalated, vec![epic.clone()], "{third:?}");
    assert_eq!(fix_count(&scratch, &epic), 3, "three attempts recorded");
    assert!(
        has_label(&scratch.show(&epic), "human"),
        "epic escalated to a human"
    );

    // Sweep 4: an escalated epic is never re-dispatched.
    let fourth = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");
    assert!(fourth.epics_seen.is_empty(), "{fourth:?}");
    assert_eq!(designer.called().len(), 3, "no further designer session");
}

#[test]
fn crash_after_complete_commit_without_projection_is_recovered() {
    let scratch = Scratch::new("crash-after-commit");
    let epic = scratch.create_epic("Crashed epic");

    // Simulate a crash right after a complete valid decomposition was committed
    // but before any gate verdict was projected: eight staged children, the
    // attempt counter, and the durable in-flight marker, no child verdicts.
    let ids: Vec<String> = (0..8)
        .map(|i| {
            scratch.create_child(
                &epic,
                &format!("child {i}"),
                &low_desc(&format!("c{i}")),
                "staged,difficulty:low",
            )
        })
        .collect();
    scratch.bd_ok(&["update", &epic, "--add-label", "decomp-attempts:1"]);
    scratch.bd_ok(&["update", &epic, "--add-label", "decomp-inflight"]);

    let designer = ScriptedDesigner::new(Mode::Valid);
    let report = sweep_epics(
        &scratch.repo,
        &scratch.state_dir,
        &gate(true, true),
        &designer,
    )
    .expect("sweep");

    // The interrupted attempt is finalized in place: no new designer session,
    // and the complete decomposition passes the gate.
    assert_eq!(report.recovered, vec![epic.clone()], "{report:?}");
    assert!(report.epics_swept.is_empty(), "no new session: {report:?}");
    assert!(designer.called().is_empty(), "designer never re-run");
    assert_eq!(report.approved, vec![epic.clone()], "{report:?}");
    assert!(
        !has_label(&scratch.show(&epic), "decomp-inflight"),
        "in-flight marker cleared"
    );
    for id in &ids {
        assert!(
            has_label(&scratch.show(id), "decomp-approved"),
            "child {id} approved by recovery"
        );
    }
    assert_eq!(staged_ids(&scratch, &epic), {
        let mut sorted = ids.clone();
        sorted.sort();
        sorted
    });
}

// ---------------------------------------------------------------------------
// Scoped single-epic entry (`agalma designer`, CP.6): one designer attempt for
// exactly one existing open epic, sharing the fleet sweep's per-epic semantics.
// ---------------------------------------------------------------------------

#[test]
fn scoped_run_decomposes_only_the_named_epic() {
    let scratch = Scratch::new("scoped-one");
    let epic1 = scratch.create_epic("Scoped epic 1");
    let epic2 = scratch.create_epic("Scoped epic 2");
    let designer = ScriptedDesigner::new(Mode::Valid);

    let report = sweep_epic(
        &scratch.repo,
        &scratch.state_dir,
        &epic1,
        &gate(true, true),
        &designer,
    )
    .expect("scoped sweep");

    assert_eq!(report.epics_seen, vec![epic1.clone()], "{report:?}");
    assert_eq!(report.approved, vec![epic1.clone()], "{report:?}");
    assert_eq!(designer.called(), vec![epic1.clone()], "only epic1 dispatched");

    // The other epic was never examined or decomposed.
    assert!(
        scratch.children(&epic2).is_empty(),
        "epic2 untouched: {:?}",
        scratch.children(&epic2)
    );

    // epic1's children are deferred, staged, and approved.
    let children = scratch.children(&epic1);
    assert_eq!(children.len(), 8, "eight staged children");
    for child in &children {
        assert!(
            has_label(child, "decomp-approved"),
            "child approved: {child}"
        );
        assert_eq!(child["status"], "deferred", "child stays deferred");
    }
}

#[test]
fn scoped_run_recovers_interrupted_attempt_without_session() {
    let scratch = Scratch::new("scoped-recover");
    let epic = scratch.create_epic("Interrupted epic");

    // A crash right after a complete valid decomposition was committed but
    // before any verdict: eight staged children, the attempt counter, and the
    // durable in-flight marker, no child verdicts.
    let ids: Vec<String> = (0..8)
        .map(|i| {
            scratch.create_child(
                &epic,
                &format!("child {i}"),
                &low_desc(&format!("c{i}")),
                "staged,difficulty:low",
            )
        })
        .collect();
    scratch.bd_ok(&["update", &epic, "--add-label", "decomp-attempts:1"]);
    scratch.bd_ok(&["update", &epic, "--add-label", "decomp-inflight"]);

    let designer = ScriptedDesigner::new(Mode::Valid);
    let report = sweep_epic(
        &scratch.repo,
        &scratch.state_dir,
        &epic,
        &gate(true, true),
        &designer,
    )
    .expect("scoped sweep");

    assert_eq!(report.recovered, vec![epic.clone()], "{report:?}");
    assert_eq!(report.approved, vec![epic.clone()], "{report:?}");
    assert!(report.epics_swept.is_empty(), "no new session: {report:?}");
    assert!(designer.called().is_empty(), "designer never re-run");
    assert!(
        !has_label(&scratch.show(&epic), "decomp-inflight"),
        "in-flight marker cleared"
    );
    assert_eq!(staged_ids(&scratch, &epic), {
        let mut sorted = ids.clone();
        sorted.sort();
        sorted
    });
}

#[test]
fn scoped_run_refuses_non_candidates_and_unknown_epics() {
    let scratch = Scratch::new("scoped-refuse");
    let designer = ScriptedDesigner::new(Mode::Valid);

    // (a) An unknown epic id is not an open epic.
    let err = sweep_epic(
        &scratch.repo,
        &scratch.state_dir,
        "sweep-nonexistent",
        &gate(true, true),
        &designer,
    )
    .expect_err("unknown epic must be refused");
    assert!(
        err.to_string().contains("not an open epic"),
        "clear reason: {err}"
    );
    assert!(designer.called().is_empty(), "no session for unknown epic");

    // (b) A human-escalated epic is excluded from open_epics: same refusal.
    let human = scratch.create_epic("Human epic");
    scratch.bd_ok(&["update", &human, "--add-label", "human"]);
    let err = sweep_epic(
        &scratch.repo,
        &scratch.state_dir,
        &human,
        &gate(true, true),
        &designer,
    )
    .expect_err("human-escalated epic must be refused");
    assert!(
        err.to_string().contains("not an open epic"),
        "clear reason: {err}"
    );
    assert!(designer.called().is_empty(), "no session for human epic");

    // (c) A fully approved epic is not a candidate: no re-dispatch.
    let epic = scratch.create_epic("Approved epic");
    let first = sweep_epic(
        &scratch.repo,
        &scratch.state_dir,
        &epic,
        &gate(true, true),
        &designer,
    )
    .expect("first scoped sweep");
    assert_eq!(first.approved, vec![epic.clone()], "{first:?}");
    assert_eq!(designer.called(), vec![epic.clone()], "one session so far");

    let err = sweep_epic(
        &scratch.repo,
        &scratch.state_dir,
        &epic,
        &gate(true, true),
        &designer,
    )
    .expect_err("approved epic must be refused");
    assert!(
        err.to_string().contains("not a decomposition candidate"),
        "clear reason: {err}"
    );
    assert_eq!(
        designer.called(),
        vec![epic.clone()],
        "no second session for an approved epic"
    );
}

#[test]
fn scoped_run_rejected_decomposition_records_fix() {
    let scratch = Scratch::new("scoped-reject");
    let epic = scratch.create_epic("Invalid epic");
    let designer = ScriptedDesigner::new(Mode::Invalid);

    let report = sweep_epic(
        &scratch.repo,
        &scratch.state_dir,
        &epic,
        &gate(true, true),
        &designer,
    )
    .expect("scoped sweep");

    assert_eq!(report.rejected, vec![epic.clone()], "{report:?}");
    assert_eq!(fix_count(&scratch, &epic), 1, "one rework task");
    assert!(report.timed_out.is_empty(), "{report:?}");
    assert!(
        !has_label(&scratch.show(&epic), "decomp-inflight"),
        "in-flight marker cleared after reconciliation"
    );
}

#[test]
fn scoped_run_is_suppressed_by_stop() {
    let scratch = Scratch::new("scoped-stop");
    let epic = scratch.create_epic("Stopped epic");
    fs::write(scratch.state_dir.join("STOP"), b"stop").expect("write STOP");
    let designer = ScriptedDesigner::new(Mode::Valid);

    let report = sweep_epic(
        &scratch.repo,
        &scratch.state_dir,
        &epic,
        &gate(true, true),
        &designer,
    )
    .expect("scoped sweep");

    assert!(
        report
            .skipped_reason
            .as_deref()
            .unwrap_or_default()
            .contains("stop policy"),
        "stop reason: {report:?}"
    );
    assert!(report.stopped, "stopped recorded: {report:?}");
    assert!(designer.called().is_empty(), "no session while stopped");
    assert!(
        scratch.children(&epic).is_empty(),
        "no children created while stopped"
    );
}
