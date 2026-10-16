//! SF.11 prerequisite-aware epic review admission and closure recheck
//! (`agalma-5fy.11`).
//!
//! Drives the real [`agalma_conductor::review`] gate against a scratch `git` +
//! `bd` workspace under `target/test-runs/sf-review-admission-*` (never `/tmp`):
//! a real repository with real commits and a real isolated `bd` store, so the
//! admitted prerequisite graph, the diff baseline/head, and the closure recheck
//! are all real. The reviewer session is a scripted [`ReviewerSession`] seam (no
//! harness, no network, no model) that can also mutate the backlog mid-review to
//! prove the recheck denies a stale approval.
//!
//! Required concrete cases: an open prerequisite waits with zero reviewer calls
//! and no drift mutation; closing it admits exactly one review/closure; a newly
//! added non-staged open child denies; a graph edit and a design edit between
//! dispatch and approval deny closure; unknown graph data denies; a benign fresh
//! observation does not invalidate; and a pre-existing genuine drift survives a
//! waiting pass. A deterministic, bounded property test independently checks
//! blocking-id derivation and semantic-input equivalence.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use agalma_conductor::review::{
    approved_snapshot_path, dispatch_hold, persist_approved_snapshot, read_approved_snapshot,
    review_epics, ReviewRequest, ReviewerSession, REVIEW_APPROVED_LABEL, REVIEW_DRIFT_LABEL,
    REVIEW_START_PREFIX,
};
use agalma_conductor::sweep::PlannerGate;
use agalma_contracts::{
    blocking_prerequisite_ids, decide_review_admission, AdmissionChild, AdmissionEdge,
    AdmissionEdgeType, EpicReviewAdmission, PackageArtifactRef, ReviewAdmissionContext,
    ReviewAdmissionDecision, EPIC_REVIEW_ADMISSION_FORMAT,
};
use agalma_taskqueue::BdTaskQueue;
use agalma_testkit::ScratchDir;
use serde_json::Value;

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(&format!("sf-review-admission-{name}"))
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

/// Select the in-process ledger binding for the review gate's stop control once,
/// so `bind_stop_control` never tries to spawn `agalma-ledger-server` from a test
/// binary that did not build the server bin (the established `sf_stop` pattern).
fn ensure_in_process_ledger() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| std::env::set_var("AGALMA_LEDGER_TRANSPORT", "in-process"));
}

/// Run `bd` against `repo`'s store (used by mid-review mutation closures).
fn bd_in(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new("bd")
        .arg("--db")
        .arg(repo.join(".beads"))
        .args(args)
        .current_dir(repo)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("spawn bd")
}

/// An isolated git + bd workspace plus a conductor state dir.
struct Scratch {
    repo: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Scratch {
    fn new(name: &str) -> Self {
        ensure_in_process_ledger();
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

    fn bd(&self, args: &[&str]) -> std::process::Output {
        bd_in(&self.repo, args)
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
            &format!("epic design for {title}"),
            "--json",
        ]);
        assert!(out.status.success(), "bd create epic: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("epic json");
        value["id"].as_str().expect("epic id").to_string()
    }

    fn create_closed_staged_child(&self, parent: &str, title: &str) -> String {
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
            &format!("child {title}"),
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

    fn create_open_plain_child(&self, parent: &str, title: &str) -> String {
        self.create_child(parent, title, "difficulty:low")
    }

    /// Create an open task child with explicit labels (kept open).
    fn create_child(&self, parent: &str, title: &str, labels: &str) -> String {
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
            &format!("child {title}"),
            "--labels",
            labels,
            "--json",
        ]);
        assert!(out.status.success(), "bd create child: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("child json");
        value["id"].as_str().expect("child id").to_string()
    }

    /// Create a closed, unlabelled non-staged child (native bd omits `labels`
    /// for such an issue).
    fn create_closed_unlabeled_child(&self, parent: &str, title: &str) -> String {
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
            &format!("child {title}"),
            "--json",
        ]);
        assert!(out.status.success(), "bd create child: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("child json");
        let id = value["id"].as_str().expect("child id").to_string();
        self.bd_ok(&["close", &id, "--reason", "landed"]);
        id
    }

    fn add_blocks_edge(&self, blocking: &str, target: &str) {
        self.bd_ok(&["dep", "add", blocking, target, "--type", "blocks"]);
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

    fn epic_labels(&self, epic: &str) -> Vec<String> {
        self.show(epic)["labels"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn children(&self, epic: &str) -> Vec<Value> {
        let stdout = self.bd_ok(&["list", "--parent", epic, "--all", "--json", "--limit", "0"]);
        serde_json::from_str(&stdout).expect("children json")
    }

    fn drift_fix_count(&self, epic: &str) -> usize {
        self.children(epic)
            .iter()
            .filter(|c| {
                c["labels"]
                    .as_array()
                    .map(|l| l.iter().any(|v| v.as_str() == Some("drift-fix")))
                    .unwrap_or(false)
            })
            .count()
    }

    /// Commit a base, create the epic with one closed staged child, advance HEAD,
    /// and record the review baseline. Returns `(epic, base_sha)`.
    fn landed_epic(&self, name: &str) -> (String, String) {
        let base = self.commit("base");
        let epic = self.create_epic(name);
        self.create_closed_staged_child(&epic, "child 0");
        self.commit("work");
        self.bd_ok(&[
            "update",
            &epic,
            "--add-label",
            &format!("{REVIEW_START_PREFIX}{base}"),
        ]);
        (epic, base)
    }

    /// The admission identity the gate would compute for `epic` at `base`.
    fn admission_record(&self, epic: &str, base: &str) -> EpicReviewAdmission {
        let queue = BdTaskQueue::new(&self.repo);
        let source = queue.review_source(epic).expect("review source");
        let head = self.git(&["rev-parse", "HEAD"]);
        let context = ReviewAdmissionContext {
            design: source.design,
            base_sha: base.to_string(),
            candidate_sha: head,
        };
        queue
            .review_admission(epic, &context)
            .expect("review admission")
    }

    fn status(&self, id: &str) -> String {
        self.show(id)["status"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

/// A captured reviewer assignment (what the reviewer was actually handed).
#[derive(Clone, Debug)]
struct CapturedRequest {
    prompt: String,
    diff_path: PathBuf,
}

/// A scripted reviewer that counts calls, captures the request it was handed,
/// and can mutate the backlog mid-review.
struct ScriptedReviewer {
    output: String,
    calls: Rc<Cell<usize>>,
    mutation: Option<Box<dyn Fn()>>,
    captured: Rc<RefCell<Option<CapturedRequest>>>,
}

impl ScriptedReviewer {
    fn new(output: &str) -> Self {
        ScriptedReviewer {
            output: output.to_string(),
            calls: Rc::new(Cell::new(0)),
            mutation: None,
            captured: Rc::new(RefCell::new(None)),
        }
    }

    fn with_mutation(output: &str, mutation: Box<dyn Fn()>) -> Self {
        ScriptedReviewer {
            output: output.to_string(),
            calls: Rc::new(Cell::new(0)),
            mutation: Some(mutation),
            captured: Rc::new(RefCell::new(None)),
        }
    }

    fn calls(&self) -> usize {
        self.calls.get()
    }

    fn captured(&self) -> Option<CapturedRequest> {
        self.captured.borrow().clone()
    }
}

impl ReviewerSession for ScriptedReviewer {
    fn run(&self, request: &ReviewRequest) -> Result<String, agalma_contracts::ContractError> {
        self.calls.set(self.calls.get() + 1);
        let prompt = fs::read_to_string(&request.prompt_path).unwrap_or_default();
        *self.captured.borrow_mut() = Some(CapturedRequest {
            prompt,
            diff_path: request.diff_path.clone(),
        });
        if let Some(mutation) = &self.mutation {
            mutation();
        }
        Ok(self.output.clone())
    }
}

fn gate() -> PlannerGate {
    PlannerGate::new(true, true, None)
}

fn is_waiting(report: &agalma_conductor::review::ReviewReport, epic: &str) -> bool {
    report.review_waiting.iter().any(|e| e == epic)
}

// ---------------------------------------------------------------------------
// Required concrete cases
// ---------------------------------------------------------------------------

#[test]
fn open_prerequisite_waits_without_review_or_drift() {
    let scratch = Scratch::new("wait");
    let (epic, _base) = scratch.landed_epic("Blocked epic");
    let prereq = scratch.create_epic("Prerequisite epic");
    scratch.add_blocks_edge(&epic, &prereq);

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(reviewer.calls(), 0, "no reviewer dispatched while waiting");
    assert!(report.epics_seen.is_empty(), "{report:?}");
    assert!(report.approved.is_empty(), "{report:?}");
    assert!(report.drift.is_empty(), "{report:?}");
    assert!(is_waiting(&report, &epic), "waiting reported: {report:?}");

    let labels = scratch.epic_labels(&epic);
    assert!(
        !labels.contains(&REVIEW_DRIFT_LABEL.to_string()),
        "{labels:?}"
    );
    assert!(
        !labels.contains(&REVIEW_APPROVED_LABEL.to_string()),
        "{labels:?}"
    );
    assert_eq!(scratch.drift_fix_count(&epic), 0, "no rework filed");
    assert_eq!(
        dispatch_hold(&BdTaskQueue::new(&scratch.repo)).expect("hold"),
        None,
        "a waiting prerequisite is not a global drift hold"
    );
}

#[test]
fn closing_prerequisite_admits_exactly_one_review_and_closure() {
    let scratch = Scratch::new("close-prereq");
    let (epic, _base) = scratch.landed_epic("Blocked epic");
    let prereq = scratch.create_epic("Prerequisite epic");
    scratch.add_blocks_edge(&epic, &prereq);

    let reviewer = ScriptedReviewer::new("done\nREVIEW: APPROVED\n");
    // First pass: waiting, zero reviewer calls.
    let first =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");
    assert!(is_waiting(&first, &epic), "{first:?}");
    assert_eq!(reviewer.calls(), 0);

    // Close the prerequisite; the very next pass admits exactly one review and
    // applies exactly one approval/closure.
    scratch.bd_ok(&["close", &prereq, "--reason", "prerequisite complete"]);
    let second =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");
    assert_eq!(reviewer.calls(), 1, "exactly one reviewer call");
    assert_eq!(second.epics_seen, vec![epic.clone()], "{second:?}");
    assert_eq!(second.approved, vec![epic.clone()], "{second:?}");
    assert_eq!(
        scratch.status(&epic),
        "closed",
        "approval actually closes the epic"
    );
    assert!(
        scratch
            .epic_labels(&epic)
            .contains(&REVIEW_APPROVED_LABEL.to_string()),
        "approved (closed) epic"
    );
    assert!(
        read_approved_snapshot(&scratch.state_dir, &epic)
            .expect("read approved identity")
            .is_some(),
        "durable approved identity persisted"
    );

    // Approval is terminal: no further review or duplicate closure is dispatched.
    let third =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");
    assert!(third.epics_seen.is_empty(), "{third:?}");
    assert!(third.approved.is_empty(), "{third:?}");
    assert_eq!(reviewer.calls(), 1, "no duplicate reviewer call");
    assert_eq!(scratch.status(&epic), "closed");
}

#[test]
fn new_non_staged_open_child_prevents_review() {
    let scratch = Scratch::new("open-child");
    let (epic, _base) = scratch.landed_epic("Epic with extra child");
    scratch.create_open_plain_child(&epic, "newly added child");

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(reviewer.calls(), 0, "an open child denies review");
    assert!(report.epics_seen.is_empty(), "{report:?}");
    assert!(report.approved.is_empty(), "{report:?}");
    let labels = scratch.epic_labels(&epic);
    assert!(
        !labels.contains(&REVIEW_APPROVED_LABEL.to_string()),
        "{labels:?}"
    );
    assert!(
        !labels.contains(&REVIEW_DRIFT_LABEL.to_string()),
        "{labels:?}"
    );
}

#[test]
fn closed_unlabeled_non_staged_child_does_not_block() {
    // Native bd omits the `labels` key for a genuinely unlabeled issue; that is
    // the documented known-empty set, not malformed data, so a closed unlabeled
    // non-staged child still admits the epic.
    let scratch = Scratch::new("unlabeled-child");
    let (epic, _base) = scratch.landed_epic("Epic with an unlabeled child");
    scratch.create_closed_unlabeled_child(&epic, "unlabeled closed child");

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(
        reviewer.calls(),
        1,
        "an unlabeled closed child still admits review"
    );
    assert_eq!(report.approved, vec![epic.clone()], "{report:?}");
    assert_eq!(scratch.status(&epic), "closed");
}

#[test]
fn graph_edit_during_review_denies_closure() {
    let scratch = Scratch::new("graph-edit");
    let (epic, _base) = scratch.landed_epic("Epic edited mid-review");
    let repo = scratch.repo.clone();
    let target = epic.clone();
    // On review, attach a brand-new open blocking prerequisite before approving.
    let mutation: Box<dyn Fn()> = Box::new(move || {
        let out = bd_in(
            &repo,
            &[
                "create",
                "Late prerequisite",
                "-t",
                "epic",
                "-p",
                "2",
                "--description",
                "late",
                "--json",
            ],
        );
        let value: Value = serde_json::from_slice(&out.stdout).expect("late epic json");
        let late = value["id"].as_str().expect("late id").to_string();
        let out = bd_in(&repo, &["dep", "add", &target, &late, "--type", "blocks"]);
        assert!(out.status.success(), "late dep: {}", stderr_of(&out));
    });
    let reviewer = ScriptedReviewer::with_mutation("REVIEW: APPROVED\n", mutation);

    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(reviewer.calls(), 1, "one paid review happened");
    assert!(
        report.approved.is_empty(),
        "stale approval not applied: {report:?}"
    );
    assert!(report.stale.iter().any(|e| e == &epic), "{report:?}");
    let labels = scratch.epic_labels(&epic);
    assert!(
        !labels.contains(&REVIEW_APPROVED_LABEL.to_string()),
        "graph edit must prevent closure: {labels:?}"
    );
}

#[test]
fn design_edit_during_review_denies_closure() {
    let scratch = Scratch::new("design-edit");
    let (epic, _base) = scratch.landed_epic("Epic redesigned mid-review");
    let repo = scratch.repo.clone();
    let target = epic.clone();
    // On review, change the epic's own source design before approving.
    let mutation: Box<dyn Fn()> = Box::new(move || {
        let out = bd_in(
            &repo,
            &[
                "update",
                &target,
                "--description",
                "the design changed after dispatch",
            ],
        );
        assert!(out.status.success(), "design edit: {}", stderr_of(&out));
    });
    let reviewer = ScriptedReviewer::with_mutation("REVIEW: APPROVED\n", mutation);

    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert!(
        report.approved.is_empty(),
        "design edit denies closure: {report:?}"
    );
    assert!(report.stale.iter().any(|e| e == &epic), "{report:?}");
    assert!(!scratch
        .epic_labels(&epic)
        .contains(&REVIEW_APPROVED_LABEL.to_string()));
}

#[test]
fn new_open_child_during_review_denies_closure() {
    let scratch = Scratch::new("child-edit");
    let (epic, _base) = scratch.landed_epic("Epic gaining a child mid-review");
    let repo = scratch.repo.clone();
    let parent = epic.clone();
    let mutation: Box<dyn Fn()> = Box::new(move || {
        let out = bd_in(
            &repo,
            &[
                "create",
                "late child",
                "-t",
                "task",
                "-p",
                "2",
                "--parent",
                &parent,
                "--description",
                "late child",
                "--json",
            ],
        );
        assert!(out.status.success(), "late child: {}", stderr_of(&out));
    });
    let reviewer = ScriptedReviewer::with_mutation("REVIEW: APPROVED\n", mutation);

    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert!(
        report.approved.is_empty(),
        "new open child denies closure: {report:?}"
    );
    assert!(report.stale.iter().any(|e| e == &epic), "{report:?}");
}

#[test]
fn benign_fresh_observation_still_approves() {
    // A waiting prerequisite that becomes eligible with no other change is
    // admitted by a fresh observation; the between-dispatch recheck is itself a
    // benign fresh read and must not invalidate the approval.
    let scratch = Scratch::new("benign");
    let (epic, _base) = scratch.landed_epic("Benign epic");
    let prereq = scratch.create_epic("Prerequisite epic");
    scratch.add_blocks_edge(&epic, &prereq);

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let waiting =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");
    assert!(is_waiting(&waiting, &epic), "{waiting:?}");

    scratch.bd_ok(&["close", &prereq, "--reason", "done"]);
    let approved =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");
    assert_eq!(approved.approved, vec![epic.clone()], "{approved:?}");
    assert!(
        approved.stale.is_empty(),
        "fresh observation is benign: {approved:?}"
    );
}

#[test]
fn existing_genuine_drift_is_preserved_while_waiting() {
    let scratch = Scratch::new("preserve-drift");
    let (epic, _base) = scratch.landed_epic("Genuinely drifting epic");
    // A real prior drift verdict: the epic carries the hold.
    scratch.bd_ok(&["update", &epic, "--add-label", REVIEW_DRIFT_LABEL]);
    let prereq = scratch.create_epic("Open prerequisite epic");
    scratch.add_blocks_edge(&epic, &prereq);

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(
        reviewer.calls(),
        0,
        "waiting prerequisite dispatches no reviewer"
    );
    assert!(is_waiting(&report, &epic), "{report:?}");
    assert!(
        scratch
            .epic_labels(&epic)
            .contains(&REVIEW_DRIFT_LABEL.to_string()),
        "genuine drift is not cleared by a waiting pass"
    );
    assert_eq!(scratch.drift_fix_count(&epic), 0, "no new rework filed");
    assert_eq!(
        dispatch_hold(&BdTaskQueue::new(&scratch.repo)).expect("hold"),
        Some(epic.clone()),
        "the genuine drift hold remains enforced"
    );
}

#[test]
fn reviewer_is_assigned_the_pinned_snapshot_and_source_edits_cannot_change_it() {
    let scratch = Scratch::new("pinned-snapshot");
    let (epic, base) = scratch.landed_epic("Pinned epic");
    let repo = scratch.repo.clone();
    let target = epic.clone();
    // Mutate the epic source during the review, then approve.
    let mutation: Box<dyn Fn()> = Box::new(move || {
        let out = bd_in(
            &repo,
            &[
                "update",
                &target,
                "--description",
                "source changed during review",
            ],
        );
        assert!(out.status.success(), "source edit: {}", stderr_of(&out));
    });
    let reviewer = ScriptedReviewer::with_mutation("REVIEW: APPROVED\n", mutation);

    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    // The reviewer was handed a prompt pinning the admitted snapshot, not live
    // backlog state.
    let captured = reviewer.captured().expect("captured request");
    assert!(
        captured.prompt.contains("admitted review snapshot:"),
        "{}",
        captured.prompt
    );
    assert!(
        !captured.prompt.contains("bd show"),
        "reviewer must not be told to re-read live backlog state: {}",
        captured.prompt
    );
    assert!(
        captured.prompt.contains(&format!("diff baseline: {base}")),
        "{}",
        captured.prompt
    );

    // The persisted snapshot matches the assignment the reviewer saw: its
    // definition bytes hash to the pinned digest.
    let dir = captured
        .diff_path
        .parent()
        .expect("attempt dir")
        .to_path_buf();
    let admission_text = fs::read_to_string(dir.join("admission.json")).expect("admission.json");
    let record: EpicReviewAdmission = serde_json::from_str(&admission_text).expect("parse record");
    record.verify_integrity().expect("integrity");
    assert!(
        captured.prompt.contains(&record.definition_digest),
        "prompt names the pinned definition digest: {}",
        captured.prompt
    );
    let definition = fs::read_to_string(dir.join("definition.json")).expect("definition.json");
    assert_eq!(
        agalma_contracts::sha256_digest(definition.as_bytes()),
        record.definition_digest,
        "persisted definition bytes hash to the pinned digest"
    );

    // The source edit between dispatch and approval withholds closure.
    assert!(report.approved.is_empty(), "{report:?}");
    assert!(report.stale.iter().any(|e| e == &epic), "{report:?}");
    assert_eq!(
        scratch.status(&epic),
        "open",
        "source edit leaves the epic open"
    );
    assert!(!scratch
        .epic_labels(&epic)
        .contains(&REVIEW_APPROVED_LABEL.to_string()));
}

#[test]
fn open_genuine_rework_child_prevents_review_until_closed() {
    // Every projected child must be closed before `Eligible`: a genuine open
    // `drift-fix` rework child blocks review, and closing it admits review.
    let scratch = Scratch::new("open-rework");
    let (epic, _base) = scratch.landed_epic("Epic with open rework");
    let fix = scratch.create_child(&epic, "drift-fix: open rework", "drift-fix");

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let blocked =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");
    assert_eq!(reviewer.calls(), 0, "an open rework child blocks review");
    assert!(blocked.epics_seen.is_empty(), "{blocked:?}");
    assert_eq!(scratch.status(&epic), "open");

    // Closing the rework child admits exactly one review/closure.
    scratch.bd_ok(&["close", &fix, "--reason", "rework complete"]);
    let admitted =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");
    assert_eq!(reviewer.calls(), 1, "closed rework admits one review");
    assert_eq!(admitted.approved, vec![epic.clone()], "{admitted:?}");
    assert_eq!(scratch.status(&epic), "closed");
}

#[test]
fn interrupted_approval_reconciles_to_closure_when_inputs_unchanged() {
    let scratch = Scratch::new("recover-approve");
    let (epic, base) = scratch.landed_epic("Recover epic");
    let record = scratch.admission_record(&epic, &base);
    persist_approved_snapshot(&scratch.state_dir, &epic, &record).expect("persist identity");
    scratch.bd_ok(&["update", &epic, "--add-label", REVIEW_APPROVED_LABEL]);
    assert_eq!(
        scratch.status(&epic),
        "open",
        "simulated interrupted approval"
    );

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(reviewer.calls(), 0, "reconciliation dispatches no reviewer");
    assert_eq!(
        scratch.status(&epic),
        "closed",
        "unchanged inputs reconcile the interrupted approval to closure"
    );
}

#[test]
fn interrupted_approval_denies_closure_after_an_edit() {
    let scratch = Scratch::new("recover-approve-edit");
    let (epic, base) = scratch.landed_epic("Recover edited epic");
    let record = scratch.admission_record(&epic, &base);
    persist_approved_snapshot(&scratch.state_dir, &epic, &record).expect("persist identity");
    scratch.bd_ok(&["update", &epic, "--add-label", REVIEW_APPROVED_LABEL]);
    // A source edit after the pinned approval identity.
    scratch.bd_ok(&["update", &epic, "--description", "edited after approval"]);

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(reviewer.calls(), 0);
    assert_eq!(
        scratch.status(&epic),
        "open",
        "edited inputs deny closure on recovery"
    );
}

#[test]
fn interrupted_approval_without_identity_denies_closure() {
    let scratch = Scratch::new("recover-no-identity");
    let (epic, _base) = scratch.landed_epic("No identity epic");
    // The terminal label alone must not close an epic: no durable identity.
    scratch.bd_ok(&["update", &epic, "--add-label", REVIEW_APPROVED_LABEL]);

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review pass");

    assert_eq!(reviewer.calls(), 0);
    assert_eq!(
        scratch.status(&epic),
        "open",
        "a label without a durable approval identity does not close the epic"
    );
}

#[test]
fn approved_snapshot_persistence_is_collision_safe() {
    let scratch = Scratch::new("approve-collision");
    let (epic, base) = scratch.landed_epic("Collision epic");
    let record = scratch.admission_record(&epic, &base);

    persist_approved_snapshot(&scratch.state_dir, &epic, &record).expect("first write");
    persist_approved_snapshot(&scratch.state_dir, &epic, &record)
        .expect("an identical rewrite is idempotent");

    // A different approval identity must be refused, preserving the original.
    let mut other = record.clone();
    other.candidate_sha = "c".repeat(40);
    other.input_digest = other.compute_input_digest().expect("digest");
    assert!(
        persist_approved_snapshot(&scratch.state_dir, &epic, &other).is_err(),
        "a differing identity must not overwrite the approved snapshot"
    );
    let stored = read_approved_snapshot(&scratch.state_dir, &epic)
        .expect("read")
        .expect("present");
    assert_eq!(
        stored.input_digest, record.input_digest,
        "the original approved identity is preserved"
    );
    assert!(approved_snapshot_path(&scratch.state_dir, &epic).exists());
}

// ---------------------------------------------------------------------------
// Unknown/missing/ambiguous graph data: the production adapter denies
// ---------------------------------------------------------------------------

/// A fake `bd` that serves per-id `show`/`list` payloads, dispatched on the
/// adapter's call shape (`show <id>` / `list --parent <id>`).
fn write_fake_bd(dir: &ScratchDir, show: &[(&str, &str)], children: &[(&str, &str)]) -> String {
    use std::os::unix::fs::PermissionsExt;
    for (id, json) in show {
        fs::write(dir.join(format!("show.{id}.json")), json).expect("write show");
    }
    for (id, json) in children {
        fs::write(dir.join(format!("children.{id}.json")), json).expect("write children");
    }
    let script = dir.join("fakebd.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nDIR=\"{dir}\"\ncase \"$3\" in\n  show) f=\"$DIR/show.$4.json\" ;;\n  \
             list) f=\"$DIR/children.$5.json\" ;;\n  *) f=\"$DIR/none.json\" ;;\nesac\n\
             if [ -f \"$f\" ]; then cat \"$f\"; else echo '[]'; fi\n",
            dir = dir.path().display(),
        ),
    )
    .expect("write fake bd");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    script.to_string_lossy().into_owned()
}

fn admission_context(dir: &Path, program: &str) -> (BdTaskQueue, ReviewAdmissionContext) {
    let queue = BdTaskQueue::new(dir).with_program(program);
    let source = queue.review_source("rev-e").expect("source");
    let context = ReviewAdmissionContext {
        design: source.design,
        base_sha: "a".repeat(40),
        candidate_sha: "b".repeat(40),
    };
    (queue, context)
}

const STAGED_CLOSED_CHILD: &str = r#"[{"id":"rev-e.1","title":"Child","status":"closed","description":"c","labels":["staged","difficulty:low"]}]"#;

#[test]
fn unknown_prerequisite_status_denies() {
    let mut dir = ScratchDir::new("sf-review-admission-unknown");
    // A `blocks` dependency with no observed status: the target is unknown.
    let show = r#"[{"id":"rev-e","title":"Epic","status":"open","description":"epic design","dependency_count":1,"dependencies":[{"id":"rev-unknown","dependency_type":"blocks"}]}]"#;
    let program = write_fake_bd(&dir, &[("rev-e", show)], &[("rev-e", STAGED_CLOSED_CHILD)]);
    let (queue, context) = admission_context(dir.path(), &program);

    let record = queue
        .review_admission("rev-e", &context)
        .expect("admission");
    assert_eq!(
        record.decision,
        ReviewAdmissionDecision::Unavailable,
        "unknown prerequisite status must deny: {record:?}"
    );
    assert!(!record.decision.is_eligible());
    dir.keep();
}

#[test]
fn omitted_dependency_projection_denies() {
    let mut dir = ScratchDir::new("sf-review-admission-omitted");
    // No `dependency_count` at all: the projection is incomplete, not empty.
    let show = r#"[{"id":"rev-e","title":"Epic","status":"open","description":"epic design"}]"#;
    let program = write_fake_bd(&dir, &[("rev-e", show)], &[("rev-e", STAGED_CLOSED_CHILD)]);
    let (queue, context) = admission_context(dir.path(), &program);

    let record = queue
        .review_admission("rev-e", &context)
        .expect("admission");
    assert_eq!(
        record.decision,
        ReviewAdmissionDecision::Unavailable,
        "omitted dependency projection must deny: {record:?}"
    );
    dir.keep();
}

#[test]
fn ambiguous_graph_data_denies() {
    let mut dir = ScratchDir::new("sf-review-admission-ambiguous");
    // A non-zero dependency count with the dependencies array absent.
    let show = r#"[{"id":"rev-e","title":"Epic","status":"open","description":"epic design","dependency_count":1}]"#;
    let program = write_fake_bd(&dir, &[("rev-e", show)], &[("rev-e", STAGED_CLOSED_CHILD)]);
    let (queue, context) = admission_context(dir.path(), &program);

    let record = queue
        .review_admission("rev-e", &context)
        .expect("admission");
    assert_eq!(
        record.decision,
        ReviewAdmissionDecision::Unavailable,
        "a non-empty count with an absent graph must deny: {record:?}"
    );
    dir.keep();
}

#[test]
fn duplicate_identical_edges_deny() {
    let mut dir = ScratchDir::new("sf-review-admission-dup-edge");
    // Two byte-identical `blocks` rows for one target: a duplicate identity.
    let show = r#"[{"id":"rev-e","title":"Epic","status":"open","description":"epic design","dependency_count":2,"dependencies":[{"id":"rev-p","dependency_type":"blocks","status":"closed"},{"id":"rev-p","dependency_type":"blocks","status":"closed"}]}]"#;
    let program = write_fake_bd(&dir, &[("rev-e", show)], &[("rev-e", STAGED_CLOSED_CHILD)]);
    let (queue, context) = admission_context(dir.path(), &program);

    let record = queue
        .review_admission("rev-e", &context)
        .expect("admission");
    assert_eq!(
        record.decision,
        ReviewAdmissionDecision::Unavailable,
        "duplicate identical edges must be rejected, not silently dropped: {record:?}"
    );
    dir.keep();
}

#[test]
fn multi_node_blocks_cycle_denies() {
    let mut dir = ScratchDir::new("sf-review-admission-cycle");
    let show_e = r#"[{"id":"rev-e","title":"Epic","status":"open","description":"epic design","dependency_count":1,"dependencies":[{"id":"rev-p","dependency_type":"blocks","status":"closed"}]}]"#;
    let show_p = r#"[{"id":"rev-p","title":"P","status":"closed","description":"p","dependency_count":1,"dependencies":[{"id":"rev-q","dependency_type":"blocks","status":"closed"}]}]"#;
    let show_q = r#"[{"id":"rev-q","title":"Q","status":"closed","description":"q","dependency_count":1,"dependencies":[{"id":"rev-e","dependency_type":"blocks","status":"closed"}]}]"#;
    let program = write_fake_bd(
        &dir,
        &[("rev-e", show_e), ("rev-p", show_p), ("rev-q", show_q)],
        &[("rev-e", STAGED_CLOSED_CHILD)],
    );
    let (queue, context) = admission_context(dir.path(), &program);

    let record = queue
        .review_admission("rev-e", &context)
        .expect("admission");
    assert_eq!(
        record.decision,
        ReviewAdmissionDecision::Unavailable,
        "a reachable multi-node blocks cycle must deny: {record:?}"
    );
    dir.keep();
}

#[test]
fn wrong_source_identity_denies() {
    let mut dir = ScratchDir::new("sf-review-admission-wrong-source");
    let show = r#"[{"id":"rev-e","title":"Epic","status":"open","description":"epic design","dependency_count":0}]"#;
    let program = write_fake_bd(&dir, &[("rev-e", show)], &[("rev-e", STAGED_CLOSED_CHILD)]);
    let (queue, mut context) = admission_context(dir.path(), &program);
    // The admitted design reference names a different source identity.
    context.design.path = "bd:rev-other:design".to_string();
    context.design.digest = format!("sha256:{}", "0".repeat(64));

    let record = queue
        .review_admission("rev-e", &context)
        .expect("admission");
    assert_eq!(
        record.decision,
        ReviewAdmissionDecision::Unavailable,
        "a wrong source identity must deny: {record:?}"
    );
    dir.keep();
}

#[test]
fn duplicate_child_identity_denies() {
    let mut dir = ScratchDir::new("sf-review-admission-dupe");
    let show = r#"[{"id":"rev-e","title":"Epic","status":"open","description":"epic design","dependency_count":0}]"#;
    // Two conflicting rows for the same child identity.
    let children = r#"[{"id":"rev-e.1","title":"Child","status":"closed","description":"c","labels":["staged"]},{"id":"rev-e.1","title":"Child","status":"open","description":"c","labels":["staged"]}]"#;
    let program = write_fake_bd(&dir, &[("rev-e", show)], &[("rev-e", children)]);
    let (queue, context) = admission_context(dir.path(), &program);

    let record = queue
        .review_admission("rev-e", &context)
        .expect("admission");
    assert_eq!(
        record.decision,
        ReviewAdmissionDecision::Unavailable,
        "duplicate identities must be rejected, not silently dropped: {record:?}"
    );
    assert!(!record.decision.is_eligible());
    dir.keep();
}

#[test]
fn eligible_fake_graph_admits() {
    // Positive control: a well-formed eligible graph is admitted (so the deny
    // cases above are not trivially denying everything).
    let mut dir = ScratchDir::new("sf-review-admission-eligible");
    let show = r#"[{"id":"rev-e","title":"Epic","status":"open","description":"epic design","dependency_count":0}]"#;
    let program = write_fake_bd(&dir, &[("rev-e", show)], &[("rev-e", STAGED_CLOSED_CHILD)]);
    let (queue, context) = admission_context(dir.path(), &program);

    let record = queue
        .review_admission("rev-e", &context)
        .expect("admission");
    assert_eq!(
        record.decision,
        ReviewAdmissionDecision::Eligible,
        "a well-formed graph is eligible: {record:?}"
    );
    record.verify_integrity().expect("integrity");
    dir.keep();
}

#[test]
fn contradictory_edge_aliases_deny() {
    let mut dir = ScratchDir::new("sf-review-admission-alias");
    // One raw edge whose kind aliases disagree (`type=tracks` vs
    // `dependency_type=blocks`) with an *open* target: resolving by preference
    // order would misclassify it as non-blocking and wrongly admit the epic.
    // A second edge whose target aliases disagree.
    let show = r#"[{"id":"rev-e","title":"Epic","status":"open","description":"epic design","dependency_count":2,"dependencies":[{"depends_on_id":"rev-open","type":"tracks","dependency_type":"blocks","status":"open"},{"depends_on_id":"rev-x","id":"rev-y","dependency_type":"blocks","status":"open"}]}]"#;
    let program = write_fake_bd(&dir, &[("rev-e", show)], &[("rev-e", STAGED_CLOSED_CHILD)]);
    let (queue, context) = admission_context(dir.path(), &program);

    let record = queue
        .review_admission("rev-e", &context)
        .expect("admission");
    assert_eq!(
        record.decision,
        ReviewAdmissionDecision::Unavailable,
        "contradictory edge aliases must deny: {record:?}"
    );
    assert!(!record.decision.is_eligible());
    dir.keep();
}

// ---------------------------------------------------------------------------
// Independent, deterministic property tests
// ---------------------------------------------------------------------------

/// A tiny deterministic generator (no external dependency): reproducible cases
/// under a fixed seed.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn sample_child(id: &str, status: &str, staged: bool) -> AdmissionChild {
    let mut labels = vec!["difficulty:low".to_string()];
    if staged {
        labels.push("staged".to_string());
    }
    // Deliberately unsorted; normalization must order labels by UTF-8.
    AdmissionChild {
        id: id.to_string(),
        definition_digest: format!("sha256:{}", "0".repeat(64)),
        status: status.to_string(),
        labels,
    }
}

fn sample_edge(target: &str, status: Option<&str>) -> AdmissionEdge {
    AdmissionEdge {
        target_id: target.to_string(),
        edge_type: AdmissionEdgeType::Blocks,
        observed_status: status.map(str::to_string),
    }
}

fn make_record(
    observed_at: u64,
    children: Vec<AdmissionChild>,
    edges: Vec<AdmissionEdge>,
    definition_digest: &str,
    design: PackageArtifactRef,
) -> EpicReviewAdmission {
    let blocking_ids = blocking_prerequisite_ids(&edges);
    let mut record = EpicReviewAdmission {
        format: EPIC_REVIEW_ADMISSION_FORMAT.to_string(),
        epic_id: "rev-e".to_string(),
        definition_digest: definition_digest.to_string(),
        design,
        base_sha: "a".repeat(40),
        candidate_sha: "b".repeat(40),
        children,
        edges,
        decision: ReviewAdmissionDecision::Eligible,
        blocking_ids,
        observed_at_unix_ms: observed_at,
        input_digest: String::new(),
    };
    record.input_digest = record.compute_input_digest().expect("digest");
    record
}

#[test]
fn semantic_inputs_ignore_observation_time_and_row_order() {
    let design = PackageArtifactRef::new("bd:rev-e:design", b"epic design", "text/plain");
    let children = vec![
        sample_child("rev-e.1", "closed", true),
        sample_child("rev-e.2", "closed", false),
    ];
    let edges = vec![sample_edge("rev-p", Some("closed"))];

    let first = make_record(
        1_000,
        children.clone(),
        edges.clone(),
        "sha256:d",
        design.clone(),
    );
    first.verify_integrity().expect("integrity");

    // Same semantic inputs, fresh observation time, reversed rows, reordered
    // labels: semantically equal but a different full-record digest.
    let mut reordered_children = children.clone();
    reordered_children.reverse();
    for child in &mut reordered_children {
        child.labels.reverse();
    }
    let mut reordered_edges = edges.clone();
    reordered_edges.reverse();
    let second = make_record(
        2_000,
        reordered_children,
        reordered_edges,
        "sha256:d",
        design.clone(),
    );
    second.verify_integrity().expect("integrity");

    assert!(
        first.semantic_inputs_equal(&second),
        "fresh observation and equivalent row order must not invalidate"
    );
    assert_ne!(
        first.input_digest, second.input_digest,
        "the full-record digest legitimately differs (timestamp/order)"
    );

    // A changed child status is a real difference.
    let mut changed_children = children.clone();
    changed_children[0].status = "open".to_string();
    let changed = make_record(
        1_000,
        changed_children,
        edges.clone(),
        "sha256:d",
        design.clone(),
    );
    assert!(!first.semantic_inputs_equal(&changed));

    // A changed design/definition is a real difference.
    let changed_source = make_record(
        1_000,
        children.clone(),
        edges.clone(),
        "sha256:different",
        design,
    );
    assert!(!first.semantic_inputs_equal(&changed_source));
}

/// One generated epic graph: children and prerequisite operations.
#[derive(Clone, Debug)]
struct GraphSpec {
    /// `(staged, closed, rework)` per child.
    children: Vec<(bool, bool, bool)>,
    /// `(target id, closed)` per prerequisite.
    prereqs: Vec<(String, bool)>,
    /// Force a structural-invalidity (Unavailable) classification.
    malformed: bool,
}

impl GraphSpec {
    /// The decision, derived from the generated intent (not the production
    /// classifier). Every projected child is implementation work, so an
    /// `Eligible` record requires every child closed (the frozen validate.py
    /// invariant), alongside at least one staged child and no open prerequisite.
    fn expected_decision(&self) -> ReviewAdmissionDecision {
        if self.malformed {
            return ReviewAdmissionDecision::Unavailable;
        }
        if self.children.is_empty() {
            return ReviewAdmissionDecision::IneligibleEmpty;
        }
        let open_prereqs = self.prereqs.iter().filter(|(_, closed)| !closed).count();
        if open_prereqs > 0 {
            return ReviewAdmissionDecision::WaitingPrerequisite;
        }
        let has_staged = self.children.iter().any(|(staged, _, _)| *staged);
        let implementation_open = self.children.iter().any(|(_, closed, _)| !*closed);
        if implementation_open || !has_staged {
            ReviewAdmissionDecision::IncompleteChildren
        } else {
            ReviewAdmissionDecision::Eligible
        }
    }

    /// The expected blocking ids, built as a set from the generated operations.
    fn expected_blocking_ids(&self) -> Vec<String> {
        let mut set = std::collections::BTreeSet::new();
        for (target, closed) in &self.prereqs {
            if !closed {
                set.insert(target.clone());
            }
        }
        set.into_iter().collect()
    }

    fn build(&self) -> (Vec<AdmissionChild>, Vec<AdmissionEdge>) {
        let children = self
            .children
            .iter()
            .enumerate()
            .map(|(index, (staged, closed, rework))| {
                let mut labels = vec!["difficulty:low".to_string()];
                if *staged {
                    labels.push("staged".to_string());
                }
                if *rework {
                    labels.push("drift-fix".to_string());
                }
                AdmissionChild {
                    id: format!("rev-e.{index}"),
                    definition_digest: format!("sha256:{}", "0".repeat(64)),
                    status: if *closed { "closed" } else { "open" }.to_string(),
                    labels,
                }
            })
            .collect();
        let edges = self
            .prereqs
            .iter()
            .map(|(target, closed)| AdmissionEdge {
                target_id: target.clone(),
                edge_type: AdmissionEdgeType::Blocks,
                observed_status: Some(if *closed { "closed" } else { "open" }.to_string()),
            })
            .collect();
        (children, edges)
    }
}

#[test]
fn admission_decision_matches_the_independent_graph_model() {
    // Fixed seed + bounded budget so any failure is replayable.
    const SEED: u64 = 0x5eed_0f11_abcd_0002;
    const CASES: usize = 4000;
    let mut rng = Lcg(SEED);

    for case in 0..CASES {
        let child_count = rng.below(5) as usize;
        let mut children = Vec::with_capacity(child_count);
        for _ in 0..child_count {
            // 0 staged closed, 1 staged open, 2 plain closed, 3 plain open,
            // 4 rework closed, 5 rework open, 6 staged+rework closed,
            // 7 staged+rework open.
            let (staged, closed, rework) = match rng.below(8) {
                0 => (true, true, false),
                1 => (true, false, false),
                2 => (false, true, false),
                3 => (false, false, false),
                4 => (false, true, true),
                5 => (false, false, true),
                6 => (true, true, true),
                _ => (true, false, true),
            };
            children.push((staged, closed, rework));
        }
        let prereq_count = rng.below(4) as usize;
        let prereqs = (0..prereq_count)
            .map(|index| (format!("rev-p.{index}"), rng.below(2) == 0))
            .collect();
        let spec = GraphSpec {
            children,
            prereqs,
            malformed: rng.below(16) == 0,
        };

        let (child_rows, edges) = spec.build();
        let decision = decide_review_admission(&child_rows, &edges, spec.malformed);
        assert_eq!(
            decision,
            spec.expected_decision(),
            "seed={SEED:#x} case={case} spec={spec:?}"
        );

        // The production blocking-id helper matches the model's expected set.
        assert_eq!(
            blocking_prerequisite_ids(&edges),
            spec.expected_blocking_ids(),
            "seed={SEED:#x} case={case} spec={spec:?}"
        );

        // A eligible decision implies non-empty, all-closed, staged children and
        // no blocking prerequisite (the validate.py invariant).
        if decision == ReviewAdmissionDecision::Eligible {
            assert!(!child_rows.is_empty());
            assert!(child_rows.iter().all(|c| c.status == "closed"));
            assert!(child_rows
                .iter()
                .any(|c| c.labels.iter().any(|l| l == "staged")));
            assert!(spec.expected_blocking_ids().is_empty());
        }
    }
}
