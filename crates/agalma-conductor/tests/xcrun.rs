//! H2 (`agalma-zxi.3`) regression: a git-based scope-guard command runs
//! through the real repo-mode acceptance launch and exits clean.
//!
//! Sandbox removal (P2) retired the Seatbelt profile, so the `xcrun_db`
//! allowance the H2 finding motivated no longer exists — the launch is
//! unconfined and `/usr/bin/git` writes its `xcrun` cache normally. This test
//! now pins the broader behavior that survived: the conductor's real `verify`
//! activity in `Live` mode clones the candidate, runs the frozen acceptance
//! command, and captures a clean green log.
//!
//! Scratch state lives under `target/test-runs/xcrun-*` (never `/tmp`); the run
//! is offline and makes no model turn (the guard is green, so no diagnosis
//! session is created). Scratch dirs are retained for review evidence
//! (`ScratchDir::keep`), not auto-removed.

#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use agalma_conductor::workspace_process::WorkspaceHandle;
use agalma_contracts::ids::{OperationId, TaskId};
use agalma_contracts::{ExecutionId, ExecutionPhase};
use agalma_execution::{ActivityNext, OperationContext};
use agalma_testkit::ScratchDir;

use agalma_conductor::harness_binding::{harness_handle, HarnessChoice};
use agalma_conductor::phases::{
    activities_with_mode, run_root, ActivityMode, PhaseDeps, RepoMode, RunState,
};

mod common;

/// The seed scope-guard from the M1 acceptance suite: green iff the candidate
/// diff touches no `Cargo.toml` and no crate `src/**/*.rs`.
const GUARD: &str =
    "sh -c '! git diff --name-only HEAD~1 HEAD | grep -qE \"Cargo\\.toml$|/src/.*\\.rs$\"'";

fn fresh(name: &str) -> ScratchDir {
    let mut dir = ScratchDir::new(&format!("xcrun-{name}"));
    // Retain fixtures for review evidence (`ScratchDir::keep`); not auto-removed.
    dir.keep();
    dir
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A scratch origin with two commits so `HEAD~1` resolves inside the
/// verification checkout. The second commit touches only `docs/`, so the
/// scope-guard is green.
fn seed_origin(dir: &Path) -> PathBuf {
    let origin = dir.join("origin");
    fs::create_dir_all(origin.join("src")).expect("origin src");
    fs::write(
        origin.join("src").join("lib.rs"),
        "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
    )
    .expect("seed source");
    git_ok(&origin, &["init", "-q", "-b", "main"]);
    git_ok(&origin, &["config", "user.email", "agalma@localhost"]);
    git_ok(&origin, &["config", "user.name", "Agalma"]);
    git_ok(&origin, &["add", "-A"]);
    git_ok(&origin, &["commit", "-qm", "seed"]);

    fs::create_dir_all(origin.join("docs")).expect("origin docs");
    fs::write(origin.join("docs").join("note.md"), "# note\n").expect("docs note");
    git_ok(&origin, &["add", "-A"]);
    git_ok(&origin, &["commit", "-qm", "docs note"]);
    origin
}

#[test]
fn repo_acceptance_git_guard_runs_clean() {
    let dir = fresh("guard");
    let origin = seed_origin(&dir);
    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    // SF.1/SF.6: repository mode requires the recorded package with a valid
    // frozen protected oracle. Record the guard as the package's frozen
    // acceptance (SF.6 prefers it over the repo acceptance) and a non-empty
    // allowed scope; the candidate is unmodified, so the guard stays green.
    common::seed_recorded_package(
        &state_dir,
        "xcrun-guard",
        1,
        &[GUARD.to_string()],
        &["docs".to_string()],
    );

    let deps = PhaseDeps {
        state_dir: state_dir.clone(),
        fixture: origin.clone(),
        model: "test/offline".to_string(),
        verifier_model: "test/offline".to_string(),
        route: "builder:fix".to_string(),
        verifier_route: "verifier:fix".to_string(),
        role: "builder".to_string(),
        builder_prompt: state_dir.join("unused-builder.md"),
        proxy_url: None,
        verifier_proxy_url: None,
        paid_proxy: false,
        toolchain_env: BTreeMap::new(),
        turn_timeout: Duration::from_secs(5),
        verify_timeout: Duration::from_secs(60),
        idle_timeout: Duration::from_secs(5),
        repo: Some(RepoMode {
            origin,
            base_ref: "main".to_string(),
            task_title: "Repo acceptance guard".to_string(),
            acceptance: vec![GUARD.to_string()],
        }),
        harness: harness_handle(HarnessChoice::OpenCode),
        stop: std::rc::Rc::new(agalma_conductor::stop::FileStopControl::new(&state_dir)),
    };

    let workspace = Rc::new(RefCell::new(WorkspaceHandle::in_process(&state_dir)));
    let state = Rc::new(RefCell::new(RunState::default()));
    let mut acts = activities_with_mode(
        deps,
        Rc::clone(&workspace),
        Rc::clone(&state),
        ActivityMode::Live,
    );

    let task_id = TaskId::new("xcrun-guard");
    let exec = ExecutionId::derive(&task_id, 1);
    let ctx = OperationContext {
        execution_id: exec.clone(),
        operation_id: OperationId::derive(&exec, "verify@1"),
        task_id,
        phase: ExecutionPhase::Verify,
        attempt: 1,
        lease_generation: 1,
        inputs: serde_json::json!({}),
    };

    // Real repo-mode checkout, then the real acceptance launch.
    acts.checkout.execute(&ctx).expect("checkout");
    let outcome = acts.verify.execute(&ctx).expect("verify execute");
    assert_eq!(
        outcome.next,
        ActivityNext::Advance,
        "scope-guard must be green: {}",
        outcome.result
    );

    let artifact = run_root(&state_dir, &exec)
        .join("artifacts")
        .join("accept@1-0.txt");
    let log = fs::read_to_string(&artifact)
        .unwrap_or_else(|e| panic!("read acceptance log {}: {e}", artifact.display()));
    assert!(log.contains("exit: 0"), "guard must exit 0:\n{log}");
}
