//! Integration tests for the bd task-queue adapter and the fenced-block parser.
//!
//! Every CLI test runs against its own scratch bd workspace under
//! `target/test-runs/taskqueue-<name>`: `git init` + `bd init --skip-agents
//! --skip-hooks`. The adapter always targets that store with `--db`, so the
//! repository's own `.beads` store is never read or written. No `/tmp`, no
//! network.

use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_contracts::{TaskFamily, TaskId, TaskPriority, TaskQueueApi};
use agalma_taskqueue::{
    parse_task_block, BdTaskQueue, DECOMP_APPROVED_LABEL, DECOMP_REJECTED_LABEL, PARKED_LABEL,
    STAGED_LABEL,
};
use agalma_testkit::ScratchDir;

// ---------------------------------------------------------------------------
// Fixtures and harness
// ---------------------------------------------------------------------------

/// A fully valid task description. Line numbers matter to the malformed tests:
/// 1 `Intro.` · 2 blank · 3 fence · 4 directive · 5 target · 6 ref ·
/// 7 acceptance · 8..9 items · 10 budget_usd · 11 priority · 12 family ·
/// 13 max_attempts · 14 close fence.
const VALID: &str = "Intro.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'true'\n  - cargo test\nbudget_usd: 2.00\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n";

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

/// An isolated bd workspace under `target/test-runs/`.
struct Scratch {
    dir: ScratchDir,
}

impl Scratch {
    fn new(name: &str) -> Self {
        require_bd();
        let dir = ScratchDir::new(&format!("taskqueue-{name}"));

        run_ok("git", &dir, &["init", "-q"]);
        run_ok("git", &dir, &["config", "user.email", "agalma@localhost"]);
        run_ok("git", &dir, &["config", "user.name", "Agalma"]);

        // cwd is the scratch repo, so bd initializes a local `.beads` store and
        // never discovers the parent repository's.
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

    fn queue(&self) -> BdTaskQueue {
        BdTaskQueue::new(self.dir.path())
    }

    /// Prevent auto-cleanup so the fixture survives the successful test for
    /// persistent evidence retention (call at the end of a required case).
    fn keep(&mut self) {
        self.dir.keep();
    }

    /// Like [`Scratch::new`], but retained even on success: required
    /// native-gate cases keep their fixture for persistent evidence.
    fn new_kept(name: &str) -> Self {
        let mut scratch = Self::new(name);
        scratch.keep();
        scratch
    }

    fn beads_dir(&self) -> PathBuf {
        self.dir.join(".beads")
    }

    /// Create a task issue with `description`; return its id.
    fn create(&self, title: &str, description: &str) -> String {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args([
                "create",
                title,
                "-t",
                "task",
                "-p",
                "2",
                "--description",
                description,
                "--json",
            ])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd create");
        assert!(out.status.success(), "bd create: {}", stderr_of(&out));
        let value: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("bd create --json");
        value["id"].as_str().expect("created id").to_string()
    }

    /// Create an issue of an explicit type (e.g. `epic`); return its id.
    fn create_typed(&self, title: &str, issue_type: &str, description: &str) -> String {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args([
                "create",
                title,
                "-t",
                issue_type,
                "-p",
                "2",
                "--description",
                description,
                "--json",
            ])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd create");
        assert!(out.status.success(), "bd create: {}", stderr_of(&out));
        let value: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("bd create --json");
        value["id"].as_str().expect("created id").to_string()
    }

    /// `bd dep add <blocked> <blocker>`: `blocker` must close before `blocked`
    /// becomes ready (bd `blocks` edges).
    fn dep_add(&self, blocked: &str, blocker: &str) {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args(["dep", "add", blocked, blocker])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd dep add");
        assert!(out.status.success(), "bd dep add: {}", stderr_of(&out));
    }

    /// Create a native human execution-approval gate blocking `blocked`
    /// (`bd gate create --type human --blocks ...` + the convention labels and
    /// `approval_for` metadata); return the gate id.
    fn add_native_gate(&self, blocked: &str) -> String {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args([
                "gate",
                "create",
                "--type",
                "human",
                "--blocks",
                blocked,
                "--reason",
                "test approval",
                "--json",
            ])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd gate create");
        assert!(out.status.success(), "bd gate create: {}", stderr_of(&out));
        let value: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("bd gate create --json");
        let gate = value["id"].as_str().expect("gate id").to_string();
        self.bd_ok(&[
            "update",
            &gate,
            "--add-label",
            "human,execution-approval",
            "--set-metadata",
            &format!("approval_for={blocked}"),
        ]);
        gate
    }

    /// Create a native gate blocking `blocked` with NO approval labels/metadata
    /// (an ordinary prerequisite gate); return the gate id.
    fn add_prerequisite_gate(&self, blocked: &str) -> String {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args([
                "gate",
                "create",
                "--type",
                "human",
                "--blocks",
                blocked,
                "--reason",
                "prerequisite",
                "--json",
            ])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd gate create");
        assert!(out.status.success(), "bd gate create: {}", stderr_of(&out));
        let value: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("gate json");
        value["id"].as_str().expect("gate id").to_string()
    }

    /// Create a task with an explicit label list; return its id (open, no defer).
    fn create_open_labeled(&self, title: &str, labels: &str) -> String {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args([
                "create",
                title,
                "-t",
                "task",
                "-p",
                "2",
                "--description",
                VALID,
                "--labels",
                labels,
                "--json",
            ])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd create");
        assert!(out.status.success(), "bd create: {}", stderr_of(&out));
        let value: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("bd create --json");
        value["id"].as_str().expect("created id").to_string()
    }

    /// Create a task with an explicit label list and a defer date.
    fn create_labeled(&self, title: &str, labels: &str, defer: &str) -> String {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args([
                "create",
                title,
                "-t",
                "task",
                "-p",
                "2",
                "--description",
                VALID,
                "--labels",
                labels,
                "--defer",
                defer,
                "--json",
            ])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd create");
        assert!(out.status.success(), "bd create: {}", stderr_of(&out));
        let value: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("bd create --json");
        value["id"].as_str().expect("created id").to_string()
    }

    /// Run a bd subcommand that must succeed; return stdout.
    fn bd_ok(&self, args: &[&str]) -> String {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args(args)
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd");
        assert!(out.status.success(), "bd {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `bd show <id> --json --include-comments`, unwrapped to the single issue.
    fn show_json(&self, id: &str) -> serde_json::Value {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads_dir())
            .args(["show", id, "--json", "--include-comments"])
            .current_dir(&self.dir)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd show");
        assert!(out.status.success(), "bd show: {}", stderr_of(&out));
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("bd show --json");
        value
            .as_array()
            .expect("show returns an array")
            .first()
            .expect("one issue")
            .clone()
    }
}

/// Replace one exact line in a description fixture.
fn replace_line(text: &str, old: &str, new: &str) -> String {
    text.lines()
        .map(|l| if l == old { new } else { l })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

#[test]
fn parses_a_valid_block() {
    let block = parse_task_block(VALID)
        .expect("no error")
        .expect("has block");
    assert_eq!(block.directive, "directive/v0");
    assert_eq!(block.target, "agalma");
    assert_eq!(block.base_ref, "main");
    assert_eq!(
        block.acceptance,
        vec!["sh -c 'true'".to_string(), "cargo test".to_string()]
    );
    assert_eq!(block.budget_usd, Some(2.0));
    assert_eq!(block.priority, TaskPriority::Normal);
    assert_eq!(block.family, TaskFamily::Fix);
    assert_eq!(block.max_attempts, 2);
    assert!(block.raw_block.contains("directive: directive/v0"));
    assert!(!block.raw_block.contains("```"));
}

#[test]
fn accepts_inline_acceptance_list() {
    let desc = replace_line(
        VALID,
        "acceptance:",
        "acceptance: ['cargo test', 'cargo fmt --check']",
    );
    let desc = desc
        .lines()
        .filter(|l| !l.starts_with("  - "))
        .collect::<Vec<_>>()
        .join("\n");
    let block = parse_task_block(&desc).unwrap().expect("block");
    assert_eq!(
        block.acceptance,
        vec!["cargo test".to_string(), "cargo fmt --check".to_string()]
    );
}

#[test]
fn missing_fence_is_not_a_task() {
    let parsed = parse_task_block("just a plain description, no fence").unwrap();
    assert!(parsed.is_none());
}

#[test]
fn unterminated_fence_is_malformed() {
    let err = parse_task_block("intro\n```yaml\ndirective: directive/v0\n").unwrap_err();
    assert_eq!(err.line, 2);
    assert!(err.message.contains("unterminated"), "{}", err.message);
}

#[test]
fn unknown_key_is_malformed_at_its_line() {
    let desc = replace_line(VALID, "ref: main", "bogus: main");
    let err = parse_task_block(&desc).unwrap_err();
    assert_eq!(err.line, 6);
    assert!(err.message.contains("unknown key"), "{}", err.message);
}

#[test]
fn bad_priority_is_malformed_at_its_line() {
    let desc = replace_line(VALID, "priority: normal", "priority: urgent");
    let err = parse_task_block(&desc).unwrap_err();
    assert_eq!(err.line, 11);
    assert!(err.message.contains("priority"), "{}", err.message);
}

#[test]
fn bad_family_is_malformed_at_its_line() {
    let desc = replace_line(VALID, "family: fix", "family: widget");
    let err = parse_task_block(&desc).unwrap_err();
    assert_eq!(err.line, 12);
    assert!(err.message.contains("family"), "{}", err.message);
}

#[test]
fn empty_acceptance_is_malformed() {
    let desc = "\
```yaml
directive: directive/v0
target: agalma
ref: main
acceptance:
budget_usd: 2.00
priority: normal
family: fix
max_attempts: 2
```
";
    let err = parse_task_block(desc).unwrap_err();
    assert!(err.message.contains("acceptance"), "{}", err.message);
}

#[test]
fn missing_required_key_is_malformed() {
    let desc = "\
```yaml
target: agalma
ref: main
acceptance:
  - cargo test
priority: normal
family: fix
max_attempts: 2
```
";
    let err = parse_task_block(desc).unwrap_err();
    assert!(
        err.message.contains("missing required key `directive`"),
        "{}",
        err.message
    );
}

#[test]
fn duplicate_key_is_malformed() {
    let desc = "\
```yaml
directive: directive/v0
directive: directive/v1
target: agalma
ref: main
acceptance:
  - cargo test
priority: normal
family: fix
max_attempts: 2
```
";
    let err = parse_task_block(desc).unwrap_err();
    assert_eq!(err.line, 3);
    assert!(err.message.contains("duplicate key"), "{}", err.message);
}

#[test]
fn zero_max_attempts_is_malformed() {
    let desc = replace_line(VALID, "max_attempts: 2", "max_attempts: 0");
    let err = parse_task_block(&desc).unwrap_err();
    assert_eq!(err.line, 13);
    assert!(err.message.contains("max_attempts"), "{}", err.message);
}

// ---------------------------------------------------------------------------
// Adapter (bd CLI, scratch store)
// ---------------------------------------------------------------------------

#[test]
fn ready_tasks_excludes_claimed_closed_parked_and_reports_nontasks() {
    let scratch = Scratch::new("ready");
    let mut queue = scratch.queue();

    let ready = scratch.create("Ready", VALID);
    let claimed = scratch.create("Claimed", VALID);
    let closed = scratch.create("Closed", VALID);
    let parked = scratch.create("Parked", VALID);
    let nontask = scratch.create("Not a task", "no fenced block here");

    queue.claim(&TaskId::new(claimed.as_str())).unwrap();
    queue
        .close_task(&TaskId::new(closed.as_str()), "done in test")
        .unwrap();
    queue
        .add_label(&TaskId::new(parked.as_str()), PARKED_LABEL)
        .unwrap();

    let intake = queue.ready_tasks().unwrap();
    assert_eq!(
        intake.tasks.len(),
        1,
        "tasks: {:?}",
        intake
            .tasks
            .iter()
            .map(|t| t.task_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(intake.tasks[0].task_id.as_str(), ready);
    assert_eq!(intake.skipped.len(), 1);
    assert_eq!(intake.skipped[0].task_id.as_str(), nontask);
    assert_eq!(
        intake.skipped[0].reason,
        agalma_contracts::SkipReason::MissingBlock
    );
}

/// Sorted task ids admitted by `ready_tasks`.
fn ready_ids(queue: &BdTaskQueue) -> Vec<String> {
    let mut ids: Vec<String> = queue
        .ready_tasks()
        .unwrap()
        .tasks
        .iter()
        .map(|t| t.task_id.as_str().to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn ready_tasks_withholds_blocked_child_until_prerequisite_closes() {
    let scratch = Scratch::new("deps-blocked");
    let mut queue = scratch.queue();

    let prereq = scratch.create("Prerequisite", VALID);
    let child = scratch.create("Blocked child", VALID);
    let independent = scratch.create("Independent", VALID);
    scratch.dep_add(&child, &prereq);

    // The blocked child is absent; the prerequisite and independent work are
    // still ready.
    let ids = ready_ids(&queue);
    assert!(ids.contains(&prereq), "prerequisite ready: {ids:?}");
    assert!(
        ids.contains(&independent),
        "independent work ready: {ids:?}"
    );
    assert!(
        !ids.contains(&child),
        "blocked child must not be admitted before its prerequisite closes: {ids:?}"
    );

    // Closing the prerequisite releases the child.
    queue
        .close_task(&TaskId::new(prereq.as_str()), "done in test")
        .unwrap();
    let ids = ready_ids(&queue);
    assert!(ids.contains(&child), "child ready after prerequisite closes: {ids:?}");
    assert!(!ids.contains(&prereq), "closed prerequisite is gone: {ids:?}");
}

#[test]
fn ready_tasks_respects_transitive_dependency_chain() {
    let scratch = Scratch::new("deps-chain");
    let mut queue = scratch.queue();

    // c depends on b depends on a: only `a` is initially ready.
    let a = scratch.create("A", VALID);
    let b = scratch.create("B", VALID);
    let c = scratch.create("C", VALID);
    scratch.dep_add(&b, &a);
    scratch.dep_add(&c, &b);

    let ids = ready_ids(&queue);
    assert_eq!(ids, vec![a.clone()], "only the chain head is ready: {ids:?}");

    queue
        .close_task(&TaskId::new(a.as_str()), "done in test")
        .unwrap();
    let ids = ready_ids(&queue);
    assert_eq!(ids, vec![b.clone()], "b ready after a closes: {ids:?}");

    queue
        .close_task(&TaskId::new(b.as_str()), "done in test")
        .unwrap();
    let ids = ready_ids(&queue);
    assert_eq!(ids, vec![c.clone()], "c ready after b closes: {ids:?}");
}

#[test]
fn ready_tasks_keeps_epic_human_staged_decomp_fix_exclusions() {
    let scratch = Scratch::new("deps-exclusions");
    let mut queue = scratch.queue();

    let ready = scratch.create("Ready", VALID);
    let epic = scratch.create_typed("Epic", "epic", VALID);
    let human = scratch.create("Human", VALID);
    let staged = scratch.create("Staged", VALID);
    let decomp = scratch.create("DecompFix", VALID);
    queue
        .add_label(&TaskId::new(human.as_str()), agalma_taskqueue::HUMAN_LABEL)
        .unwrap();
    queue
        .add_label(&TaskId::new(staged.as_str()), agalma_taskqueue::STAGED_LABEL)
        .unwrap();
    queue
        .add_label(
            &TaskId::new(decomp.as_str()),
            agalma_taskqueue::DECOMP_FIX_LABEL,
        )
        .unwrap();

    let ids = ready_ids(&queue);
    assert_eq!(ids, vec![ready.clone()], "only ordinary work is ready: {ids:?}");
    for excluded in [&epic, &human, &staged, &decomp] {
        assert!(
            !ids.contains(excluded),
            "excluded issue {excluded} was admitted: {ids:?}"
        );
    }
}

#[test]
fn get_returns_the_parsed_task() {
    let scratch = Scratch::new("get");
    let queue = scratch.queue();
    let id = scratch.create("Gettable", VALID);

    let task = queue.get(&TaskId::new(id.as_str())).unwrap();
    assert_eq!(task.task_id.as_str(), id);
    assert_eq!(task.title, "Gettable");
    assert_eq!(task.acceptance.len(), 2);
    assert_eq!(task.priority, TaskPriority::Normal);
    assert_eq!(task.family, TaskFamily::Fix);
}

#[test]
fn claim_flips_status_and_returns_evidence() {
    let scratch = Scratch::new("claim");
    let mut queue = scratch.queue();
    let id = scratch.create("Claimable", VALID);

    let evidence = queue.claim(&TaskId::new(id.as_str())).unwrap();
    assert_eq!(evidence.task_id.as_str(), id);
    assert_eq!(evidence.status, "in_progress");

    let shown = scratch.show_json(&id);
    assert_eq!(shown["status"], "in_progress");
    assert!(shown["assignee"].is_string());
}

#[test]
fn comment_close_label_are_visible_via_show() {
    let scratch = Scratch::new("projection");
    let mut queue = scratch.queue();
    let id = scratch.create("Projected", VALID);
    let task = TaskId::new(id.as_str());

    queue
        .comment(&task, "phase=build execution=exec:x:1 lease=1")
        .unwrap();
    queue.add_label(&task, PARKED_LABEL).unwrap();
    queue.close_task(&task, "done: green").unwrap();

    let shown = scratch.show_json(&id);
    assert_eq!(shown["status"], "closed");
    assert_eq!(shown["close_reason"], "done: green");
    assert!(
        shown["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == PARKED_LABEL),
        "parked label missing: {shown}"
    );
    assert!(
        shown["comments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["text"] == "phase=build execution=exec:x:1 lease=1"),
        "comment missing: {shown}"
    );
}

#[test]
fn malformed_block_reports_issue_id_and_line() {
    let scratch = Scratch::new("malformed");
    let queue = scratch.queue();
    let bad = replace_line(VALID, "family: fix", "family: widget");
    let id = scratch.create("Malformed", &bad);

    let err = queue.get(&TaskId::new(id.as_str())).unwrap_err();
    let message = err.to_string();
    assert!(message.contains(&id), "id missing from {message}");
    assert!(message.contains("line 12"), "line missing from {message}");

    let err = queue.ready_tasks().unwrap_err();
    let message = err.to_string();
    assert!(message.contains(&id), "id missing from {message}");
    assert!(message.contains("line 12"), "line missing from {message}");
}

// ---------------------------------------------------------------------------
// Native human approval gates (agalma-i5h.8)
// ---------------------------------------------------------------------------

#[test]
fn open_staged_children_have_no_sentinel_date_and_one_native_gate() {
    let scratch = Scratch::new_kept("native-gate-open");
    let queue = scratch.queue();

    let child = scratch.create_open_labeled(
        "Child",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
    );
    let gate = scratch.add_native_gate(&child);

    // The child is open, has no defer date, and its gate is a native human gate
    // with the convention labels/metadata and a direct `blocks` edge.
    let child_json = scratch.show_json(&child);
    assert_eq!(child_json["status"], "open", "child is open: {child_json}");
    assert!(
        child_json.get("defer_until").is_none() || child_json["defer_until"].is_null(),
        "no sentinel defer date: {child_json}"
    );
    let gate_json = scratch.show_json(&gate);
    assert_eq!(gate_json["issue_type"], "gate", "native gate type");
    assert_eq!(gate_json["await_type"], "human", "native human await type");
    assert_eq!(gate_json["metadata"]["approval_for"], child);
    let labels: Vec<&str> = gate_json["labels"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l.as_str())
        .collect();
    assert!(labels.contains(&"human"), "human label: {labels:?}");
    assert!(
        labels.contains(&"execution-approval"),
        "execution-approval label: {labels:?}"
    );

    // Validation alone does not release; the unresolved gate keeps it hidden.
    assert!(
        ready_ids(&queue).is_empty(),
        "unresolved native gate keeps an approved child hidden"
    );
    // bd's own ready query also withholds it (the open gate is a real blocker),
    // proving the native gate — not just the adapter predicate — pins the child.
    let raw = scratch.bd_ok(&["list", "--ready", "--json", "--limit", "0"]);
    let raw: serde_json::Value = serde_json::from_str(&raw).expect("bd ready json");
    let raw_ids: Vec<&str> = raw
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|i| i["id"].as_str())
        .collect();
    assert!(
        !raw_ids.contains(&child.as_str()),
        "an unresolved native gate blocks the child in bd's own ready query: {raw_ids:?}"
    );
    assert!(
        !raw_ids.contains(&gate.as_str()),
        "the gate issue is not surfaced as ordinary work: {raw_ids:?}"
    );
}

/// A staged child releases only after validator approval AND a resolved native
/// gate; a rejected or never-validated child never releases.
#[test]
fn staged_child_releases_only_after_validator_approval_and_resolved_gate() {
    let scratch = Scratch::new_kept("native-gate-release");
    let queue = scratch.queue();

    let ready = scratch.create("Ready", VALID);
    let approved = scratch.create_open_labeled(
        "Approved",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
    );
    let approved_gate = scratch.add_native_gate(&approved);
    let rejected = scratch.create_open_labeled(
        "Rejected",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_REJECTED_LABEL}"),
    );
    let rejected_gate = scratch.add_native_gate(&rejected);
    let unvalidated =
        scratch.create_open_labeled("Unvalidated", &format!("{STAGED_LABEL},difficulty:low"));
    let unvalidated_gate = scratch.add_native_gate(&unvalidated);

    // Nothing staged is released while every gate is unresolved.
    assert_eq!(ready_ids(&queue), vec![ready.clone()], "all gates unresolved");

    // Resolve only the approved child's gate: exactly it releases.
    scratch.bd_ok(&["gate", "resolve", &approved_gate]);
    let mut expected = vec![ready.clone(), approved.clone()];
    expected.sort();
    assert_eq!(
        ready_ids(&queue),
        expected,
        "approved child ready after its gate resolves"
    );

    // Resolving the rejected/unvalidated gates does not release them.
    scratch.bd_ok(&["gate", "resolve", &rejected_gate]);
    scratch.bd_ok(&["gate", "resolve", &unvalidated_gate]);
    assert_eq!(
        ready_ids(&queue),
        expected,
        "rejected/unvalidated children stay hidden even with a resolved gate"
    );

    // A rebuilt adapter (fresh `BdTaskQueue`) sees the same durable state.
    let rebuilt = scratch.queue();
    assert_eq!(
        ready_ids(&rebuilt),
        expected,
        "approval survives a rebuild"
    );
}

/// Missing, conflicting, and malformed gate associations all deny release.
#[test]
fn missing_conflicting_and_malformed_gates_deny() {
    let scratch = Scratch::new_kept("native-gate-faults");
    let queue = scratch.queue();

    // Missing: approved but no gate at all.
    let orphan = scratch.create_open_labeled(
        "Orphan",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
    );

    // Conflicting: two resolved native gates.
    let ambiguous = scratch.create_open_labeled(
        "Ambiguous",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
    );
    let g1 = scratch.add_native_gate(&ambiguous);
    let g2 = scratch.add_native_gate(&ambiguous);
    scratch.bd_ok(&["gate", "resolve", &g1]);
    scratch.bd_ok(&["gate", "resolve", &g2]);

    // Malformed target: a native gate whose approval_for names a different task.
    let wrong_target = scratch.create_open_labeled(
        "WrongTarget",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
    );
    let g3 = scratch.add_native_gate(&wrong_target);
    scratch.bd_ok(&["update", &g3, "--set-metadata", "approval_for=someone-else"]);
    scratch.bd_ok(&["gate", "resolve", &g3]);

    // Non-native impostor: a plain task that copied the labels/metadata and a
    // blocks edge, then closed.
    let impostor_child = scratch.create_open_labeled(
        "ImpostorChild",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
    );
    let impostor = scratch.create_open_labeled("Impostor", "human,execution-approval");
    scratch.bd_ok(&["update", &impostor, "--set-metadata", &format!("approval_for={impostor_child}")]);
    scratch.bd_ok(&["dep", "add", &impostor_child, &impostor]);
    scratch.bd_ok(&["close", &impostor, "--reason", "not a native gate"]);

    assert!(
        ready_ids(&queue).is_empty(),
        "every fault association denies release: {orphan} {ambiguous} {wrong_target} {impostor_child}"
    );
}

/// Discovery must not silently reduce a duplicate gate to a single association
/// when the second linked native gate has lost its `execution-approval` label:
/// both are linked to the child with the same `approval_for`, so the exact-one
/// rule denies even though one of them is a valid resolved gate.
#[test]
fn duplicate_gate_with_stripped_label_denies() {
    let scratch = Scratch::new_kept("native-gate-stripped-label");
    let queue = scratch.queue();

    let child = scratch.create_open_labeled(
        "Child",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
    );

    // A valid, resolved, labelled native gate.
    let valid = scratch.add_native_gate(&child);
    scratch.bd_ok(&["gate", "resolve", &valid]);

    // A second native gate linked to the same child, with `approval_for` set but
    // its `execution-approval` label stripped, then resolved.
    let stripped = Command::new("bd")
        .arg("--db")
        .arg(scratch.beads_dir())
        .args([
            "gate",
            "create",
            "--type",
            "human",
            "--blocks",
            &child,
            "--reason",
            "second approval",
            "--json",
        ])
        .current_dir(scratch.dir.path())
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("spawn bd gate create");
    assert!(
        stripped.status.success(),
        "bd gate create: {}",
        stderr_of(&stripped)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&stripped.stdout).expect("gate json");
    let stripped = value["id"].as_str().expect("gate id").to_string();
    scratch.bd_ok(&["update", &stripped, "--set-metadata", &format!("approval_for={child}")]);
    scratch.bd_ok(&["gate", "resolve", &stripped]);

    // Both blockers are closed, so bd's own ready query surfaces the child; the
    // adapter must still deny it (ambiguous duplicate association).
    let raw: serde_json::Value =
        serde_json::from_str(&scratch.bd_ok(&["list", "--ready", "--json", "--limit", "0"]))
            .expect("bd ready json");
    let raw_ids: Vec<&str> = raw
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|i| i["id"].as_str())
        .collect();
    assert!(
        raw_ids.contains(&child.as_str()),
        "both blockers are closed so bd surfaces the child: {raw_ids:?}"
    );
    assert!(
        ready_ids(&queue).is_empty(),
        "a stripped-label duplicate gate is a conflicting association and denies release"
    );
}

/// An unrelated native prerequisite gate (no execution-approval claim) blocks
/// the child like any dependency, then permits readiness once closed alongside
/// the resolved execution-approval gate.
#[test]
fn unrelated_native_prerequisite_gate_blocks_then_allows_readiness() {
    let scratch = Scratch::new_kept("native-gate-unrelated");
    let queue = scratch.queue();

    let child = scratch.create_open_labeled(
        "Child",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
    );
    let approval = scratch.add_native_gate(&child);
    scratch.bd_ok(&["gate", "resolve", &approval]);
    // A separate native gate that claims no approval: an ordinary prerequisite.
    let prereq = scratch.add_prerequisite_gate(&child);

    // The unrelated prerequisite is still open: the child is withheld even
    // though its execution-approval gate is resolved.
    assert!(
        ready_ids(&queue).is_empty(),
        "an open unrelated prerequisite gate withholds the child"
    );

    // Closing the unrelated prerequisite releases the child alongside its
    // resolved execution-approval gate (ordinary gate/dependency semantics).
    scratch.bd_ok(&["gate", "resolve", &prereq]);
    assert_eq!(
        ready_ids(&queue),
        vec![child.clone()],
        "closing the unrelated prerequisite allows readiness"
    );
}

/// A genuine postponement keeps its defer semantics and is not authorization;
/// undefer only makes a postponed task schedulable, and a staged child still
/// needs a resolved gate.
#[test]
fn genuine_deferral_retained_and_undefer_is_not_authorization() {
    let scratch = Scratch::new_kept("native-gate-defer");
    let queue = scratch.queue();

    // Ordinary deferred work: `undefer` releases it (defer semantics retained).
    let postponed = scratch.create_labeled("Postponed", "difficulty:low", "2099-01-01");
    assert!(ready_ids(&queue).is_empty(), "deferred work is hidden");
    scratch.bd_ok(&["undefer", &postponed]);
    assert_eq!(
        ready_ids(&queue),
        vec![postponed.clone()],
        "undefer releases ordinary postponed work"
    );

    // A staged, approved, gate-resolved child that is also deferred stays
    // hidden until its real postponement is cleared — and clearing the date
    // alone (undefer) is not what authorizes it: the gate is.
    let staged = scratch.create_labeled(
        "StagedPostponed",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
        "2099-01-01",
    );
    let gate = scratch.add_native_gate(&staged);
    // Undefer only: real postponement cleared, but the gate is still open.
    scratch.bd_ok(&["undefer", &staged]);
    assert!(
        !ready_ids(&queue).contains(&staged),
        "undefer alone never authorizes a staged child"
    );
    // Resolve the gate: now (and only now) the child releases.
    scratch.bd_ok(&["gate", "resolve", &gate]);
    assert!(
        ready_ids(&queue).contains(&staged),
        "resolved gate plus cleared postponement releases the child"
    );

    // A second staged, approved, gate-resolved child that is deferred for a real
    // reason stays hidden: a real postponement is respected.
    let still_deferred = scratch.create_labeled(
        "StillDeferred",
        &format!("{STAGED_LABEL},difficulty:low,{DECOMP_APPROVED_LABEL}"),
        "2099-01-01",
    );
    let gate2 = scratch.add_native_gate(&still_deferred);
    scratch.bd_ok(&["gate", "resolve", &gate2]);
    assert!(
        !ready_ids(&queue).contains(&still_deferred),
        "a resolved gate is not authorization past a genuine postponement"
    );
}
