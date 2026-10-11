//! CP.4 concierge intake end-to-end tests (`agalma-i5h.5`).
//!
//! Each test drives the real [`agalma_conductor::concierge::run_concierge`]
//! against a scratch `bd` workspace under `target/test-runs/concierge-*` (never
//! `/tmp`): `git init` + `bd init --skip-agents --skip-hooks`. The concierge
//! session is a scripted [`ConciergeSession`] seam (no harness, no network); the
//! route decision, output-contract parsing, and epic verification are real.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_contracts::ContractError;
use agalma_contracts::LedgerApi;
use agalma_conductor::cli::{Cli, Command as CliCommand};
use agalma_conductor::concierge::{run_concierge, ConciergeRequest, ConciergeSession};
use agalma_conductor::sweep::PlannerGate;
use clap::Parser;
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
        .join(format!("concierge-{name}-{}-{n}", std::process::id()));
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
                "concierge",
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

    fn bd(&self, args: &[&str]) -> std::process::Output {
        Command::new("bd")
            .arg("--db")
            .arg(self.repo.join(".beads"))
            .args(args)
            .current_dir(&self.repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd")
    }

    fn show(&self, id: &str) -> Value {
        let out = self.bd(&["show", id, "--json", "--include-comments"]);
        assert!(out.status.success(), "bd show {id}: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("show json");
        value
            .as_array()
            .expect("show array")
            .first()
            .expect("one issue")
            .clone()
    }
}

/// A scripted concierge: creates (or fails to create) an epic in the target repo
/// and returns the output contract the conductor parses.
struct ScriptedConcierge {
    behavior: Behavior,
    called: RefCell<Vec<String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    /// Create an epic with a non-empty design; emit a full contract.
    EpicWithDesign,
    /// Create an epic but never write a design; emit the epic line anyway.
    EpicWithoutDesign,
    /// Emit no `epic:` line at all.
    MissingEpicLine,
}

impl ScriptedConcierge {
    fn new(behavior: Behavior) -> Self {
        ScriptedConcierge {
            behavior,
            called: RefCell::new(Vec::new()),
        }
    }

    fn called(&self) -> Vec<String> {
        self.called.borrow().clone()
    }
}

impl ConciergeSession for ScriptedConcierge {
    fn run(&self, request: &ConciergeRequest) -> Result<String, ContractError> {
        self.called.borrow_mut().push(request.goal.clone());
        let repo = &request.repo;
        let bd = |args: &[&str]| {
            let out = Command::new("bd")
                .arg("--db")
                .arg(repo.join(".beads"))
                .args(args)
                .current_dir(repo)
                .env("BD_NON_INTERACTIVE", "1")
                .output()
                .expect("spawn bd in concierge");
            assert!(out.status.success(), "concierge bd {args:?}: {}", stderr_of(&out));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let create_epic = |with_design: bool| {
            let mut args = vec![
                "create",
                "Goal epic",
                "-t",
                "epic",
                "-p",
                "2",
                "--description",
                "epic from a stated goal",
                "--json",
            ];
            if with_design {
                args.extend(["--design", "the epic design"]);
            }
            let stdout = bd(&args);
            let value: Value = serde_json::from_str(&stdout).expect("epic json");
            value["id"].as_str().expect("epic id").to_string()
        };
        match self.behavior {
            Behavior::EpicWithDesign => {
                let id = create_epic(true);
                Ok(format!(
                    "epic: {id}\ndesign: written to {id} --design\nhandoff: bd show {id}\n"
                ))
            }
            Behavior::EpicWithoutDesign => {
                let id = create_epic(false);
                Ok(format!(
                    "epic: {id}\ndesign: written to {id} --design\nhandoff: bd show {id}\n"
                ))
            }
            Behavior::MissingEpicLine => {
                Ok("design: written nowhere\nhandoff: bd show nothing\n".to_string())
            }
        }
    }
}

fn gate(allow_paid: bool, paid: bool) -> PlannerGate {
    PlannerGate::new(allow_paid, paid, None)
}

// ---------------------------------------------------------------------------

#[test]
fn scripted_intake_reports_the_epic_id() {
    let scratch = Scratch::new("intake");
    let session = ScriptedConcierge::new(Behavior::EpicWithDesign);

    let report = run_concierge(
        &scratch.repo,
        &scratch.state_dir,
        "stand up a TUI dashboard",
        &gate(true, true),
        &session,
    )
    .expect("concierge intake");

    assert_eq!(report.epics.len(), 1, "one epic reported: {report:?}");
    let epic = &report.epics[0];
    assert_eq!(session.called(), vec!["stand up a TUI dashboard".to_string()]);

    // The epic exists and carries a non-empty design.
    let issue = scratch.show(epic);
    assert!(
        !issue["design"].as_str().unwrap_or("").trim().is_empty(),
        "epic design present: {issue}"
    );

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
fn output_missing_epic_line_is_a_clear_error() {
    let scratch = Scratch::new("missing-line");
    let session = ScriptedConcierge::new(Behavior::MissingEpicLine);

    let err = run_concierge(
        &scratch.repo,
        &scratch.state_dir,
        "vague goal",
        &gate(true, true),
        &session,
    )
    .expect_err("missing epic line must fail");

    assert!(err.to_string().contains("epic:"), "clear error: {err}");
}

#[test]
fn epic_without_design_is_a_clear_error() {
    let scratch = Scratch::new("no-design");
    let session = ScriptedConcierge::new(Behavior::EpicWithoutDesign);

    let err = run_concierge(
        &scratch.repo,
        &scratch.state_dir,
        "goal without design",
        &gate(true, true),
        &session,
    )
    .expect_err("epic lacking a design must fail");

    assert!(err.to_string().contains("design"), "clear error: {err}");
}

#[test]
fn planner_unavailable_fails_closed_without_a_session() {
    let scratch = Scratch::new("unavailable");
    let session = ScriptedConcierge::new(Behavior::EpicWithDesign);

    // AGALMA_ALLOW_PAID off: the planner route is unavailable.
    let err = run_concierge(
        &scratch.repo,
        &scratch.state_dir,
        "blocked goal",
        &gate(false, true),
        &session,
    )
    .expect_err("unavailable planner must fail closed");

    assert!(
        err.to_string().contains("never downgrading"),
        "clear reason: {err}"
    );
    assert!(session.called().is_empty(), "no session was spawned");

    // No route decision is recorded either.
    let ledger = scratch.state_dir.join("ledger.sqlite");
    assert!(!ledger.exists(), "no session, no ledger: {}", ledger.display());
}

// ---------------------------------------------------------------------------
// Stop policy (`agalma-r2l`): an explicit concierge intake obeys the same STOP
// sentinel + durable kill latch as the automatic sweep.
// ---------------------------------------------------------------------------

#[test]
fn concierge_intake_blocked_by_stop_sentinel() {
    let scratch = Scratch::new("stopped");
    fs::write(scratch.state_dir.join("STOP"), b"stop").expect("write STOP");
    let session = ScriptedConcierge::new(Behavior::EpicWithDesign);

    let err = run_concierge(
        &scratch.repo,
        &scratch.state_dir,
        "stand up a TUI",
        &gate(true, true),
        &session,
    )
    .expect_err("a stop must suppress the intake");

    assert!(err.to_string().contains("stop policy"), "clear reason: {err}");
    assert!(session.called().is_empty(), "no session spawned while stopped");
    // The stop is never cleared implicitly by the intake.
    assert!(scratch.state_dir.join("STOP").exists(), "STOP retained");
    // No route decision was recorded before the stop check.
    assert!(
        !scratch.state_dir.join("ledger.sqlite").exists(),
        "no ledger/route recorded while stopped"
    );
}

#[test]
fn concierge_intake_blocked_by_kill_latch() {
    let scratch = Scratch::new("latched");
    {
        let mut ledger =
            agalma_ledger::SqliteLedger::open(scratch.state_dir.join("ledger.sqlite"))
                .expect("open ledger");
        ledger.set_kill_latch(true).expect("set latch");
    }
    assert!(
        !scratch.state_dir.join("STOP").exists(),
        "STOP absent; only the latch is set"
    );
    let session = ScriptedConcierge::new(Behavior::EpicWithDesign);

    let err = run_concierge(
        &scratch.repo,
        &scratch.state_dir,
        "stand up a TUI",
        &gate(true, true),
        &session,
    )
    .expect_err("a kill latch must suppress the intake");

    assert!(err.to_string().contains("stop policy"), "clear reason: {err}");
    assert!(session.called().is_empty(), "no session spawned");
    let ledger = agalma_ledger::SqliteLedger::open(scratch.state_dir.join("ledger.sqlite"))
        .expect("reopen ledger");
    assert!(
        ledger.kill_latch().expect("read latch"),
        "latch retained (never implicitly cleared)"
    );
}

#[test]
fn concierge_arg_and_help_wiring() {
    // In-process parse: goal is positional; repo/model/state-dir are flags.
    let cli = Cli::try_parse_from([
        "agalma",
        "concierge",
        "build a dashboard",
        "--repo",
        "/some/repo",
        "--model",
        "opencode-go/deepseek-v4-pro",
    ])
    .expect("parse concierge args");
    match cli.command {
        CliCommand::Concierge(args) => {
            assert_eq!(args.goal, "build a dashboard");
            assert_eq!(args.repo.as_deref(), Some(Path::new("/some/repo")));
            assert_eq!(
                args.model.as_deref(),
                Some("opencode-go/deepseek-v4-pro")
            );
        }
        _ => panic!("expected Concierge command"),
    }

    // The binary advertises the subcommand.
    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args(["concierge", "--help"])
        .output()
        .expect("run agalma concierge --help");
    assert!(out.status.success(), "help exit: {}", stderr_of(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("GOAL"), "help lists the goal arg: {stdout}");
    assert!(stdout.contains("--repo"), "help lists --repo: {stdout}");
}
