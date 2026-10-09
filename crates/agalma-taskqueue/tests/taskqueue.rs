//! Integration tests for the bd task-queue adapter and the fenced-block parser.
//!
//! Every CLI test runs against its own scratch bd workspace under
//! `target/test-runs/taskqueue-<name>`: `git init` + `bd init --skip-agents
//! --skip-hooks`. The adapter always targets that store with `--db`, so the
//! repository's own `.beads` store is never read or written. No `/tmp`, no
//! network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_contracts::{TaskFamily, TaskId, TaskPriority, TaskQueueApi};
use agalma_taskqueue::{parse_task_block, BdTaskQueue, PARKED_LABEL};

// ---------------------------------------------------------------------------
// Fixtures and harness
// ---------------------------------------------------------------------------

/// A fully valid task description. Line numbers matter to the malformed tests:
/// 1 `Intro.` · 2 blank · 3 fence · 4 directive · 5 target · 6 ref ·
/// 7 acceptance · 8..9 items · 10 budget_usd · 11 priority · 12 family ·
/// 13 max_attempts · 14 close fence.
const VALID: &str = "Intro.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'true'\n  - cargo test\nbudget_usd: 2.00\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
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

/// An isolated bd workspace under `target/test-runs/`.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        require_bd();
        let dir = root()
            .join("target")
            .join("test-runs")
            .join(format!("taskqueue-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create run dir");

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
        BdTaskQueue::new(&self.dir)
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
