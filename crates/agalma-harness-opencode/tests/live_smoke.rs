//! Live (gated) smoke test for the OpenCode harness adapter.
//!
//! Ignored by default. Run it explicitly with the live flag set:
//!
//! ```text
//! AGALMA_HARNESS_LIVE=1 cargo test -p agalma-harness-opencode \
//!     --test live_smoke -- --ignored --nocapture
//! ```
//!
//! It starts an attempt confined by [`agalma_sandbox::SeatbeltSandbox`], creates
//! sessions, runs a free-model turn to a terminal state, cancels a second turn,
//! stops the attempt with evidence, and asserts no confined process survives and
//! that the user's OpenCode state was not touched. All scratch state lives under
//! `target/test-runs/harness-live-*` (never `/tmp`).

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_contracts::harness::{
    CreateSessionRequest, Event, HarnessApi, OperationHandle, OperationState, RunTurnRequest,
    StartAttemptRequest,
};
use agalma_contracts::ids::{ArtifactRef, OperationId};
use agalma_harness_opencode::{OpenCodeHarness, OpenCodeHarnessConfig};
use agalma_sandbox::{SeatbeltSandbox, PRODUCT_PROFILE};

fn live_enabled() -> bool {
    std::env::var("AGALMA_HARNESS_LIVE").ok().as_deref() == Some("1")
}

fn run_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("test-runs");
    let root = base.join(format!("harness-live-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("create run root");
    root
}

fn pid_alive(pid: u32) -> bool {
    match Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
    {
        Ok(out) if out.status.success() => true,
        Ok(out) => String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"),
        Err(_) => false,
    }
}

/// Pids still in process group `pgid` (should be empty after termination).
fn group_members(pgid: u32) -> Vec<u32> {
    Command::new("/usr/bin/pgrep")
        .arg("-g")
        .arg(pgid.to_string())
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn poll_terminal(
    harness: &mut OpenCodeHarness,
    op: &OperationHandle,
    timeout: Duration,
) -> OperationState {
    let deadline = Instant::now() + timeout;
    loop {
        let state = harness.inspect_operation(op).expect("inspect");
        if state.is_terminal() {
            return state;
        }
        if Instant::now() >= deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn live_attempt_turn_cancel_stop_no_orphans() {
    if !live_enabled() {
        eprintln!("live smoke skipped: set AGALMA_HARNESS_LIVE=1 to run");
        return;
    }

    let root = run_root();
    let attempt_dir = root.join("attempt");
    let protected = root.join("protected");
    let sock = root.join("sock");
    let workspace = attempt_dir.join("work");
    for dir in [&attempt_dir, &protected, &sock, &workspace] {
        fs::create_dir_all(dir).expect("create attempt dir");
    }
    let profile = root.join("worker.sb");
    fs::write(&profile, PRODUCT_PROFILE).expect("write profile");

    // The real build workspace is a git checkout (`WorkspaceApi::prepare` runs
    // `git init`); opencode's project discovery resolves the git root, so the
    // smoke mirrors that instead of letting discovery climb to the outer repo.
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&workspace)
        .status()
        .expect("git init");
    assert!(status.success(), "git init workspace");

    // User OpenCode state must stay untouched.
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    let user_config = home.join(".config").join("opencode");
    let user_data = home.join(".local").join("share").join("opencode");
    let before_config = mtime(&user_config);
    let before_data = mtime(&user_data);

    eprintln!("live smoke root: {}", root.display());
    let mut config = OpenCodeHarnessConfig::new(&profile, &attempt_dir, &protected, &sock);
    // The adapter points OpenCode's global config dir at the worktree, which
    // short-circuits its ancestor config walk (see `spawn_server`). This optional
    // override exists for hosts where a broader read root is needed instead.
    if let Some(extra) = std::env::var_os("AGALMA_HARNESS_EXTRA_RO") {
        config.extra_ro = Some(PathBuf::from(extra));
    }
    let mut harness = OpenCodeHarness::new(Box::new(SeatbeltSandbox::new()), config);

    let describe = harness.describe();
    eprintln!(
        "describe: {} {} api v{} caps={:?}",
        describe.impl_name, describe.impl_version, describe.api_version, describe.capabilities
    );
    assert_eq!(describe.impl_name, "opencode");

    let attempt = harness
        .start_attempt(StartAttemptRequest {
            workspace: workspace.to_string_lossy().into_owned(),
            role: "builder".to_string(),
            genome_ref: ArtifactRef::derive("genome").to_string(),
            constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
            limits: agalma_contracts::harness::Limits {
                wall_ms: 300_000,
                tokens: 20_000,
                cost_micros: 0,
            },
            operation_id: OperationId::new("op:live:start"),
        })
        .expect("start_attempt in sandbox");
    let root_pid = harness.attempt_pid().expect("server pid");
    eprintln!("server pid: {root_pid}");

    // --- first session + turn to a terminal state --------------------------
    let session = harness
        .create_session(CreateSessionRequest {
            role: "builder".to_string(),
            model: "role-default".to_string(),
            prompts_ref: ArtifactRef::derive("builder-prompts").to_string(),
            handoff_refs: vec![],
            tool_policy_ref: ArtifactRef::derive("tool-policy").to_string(),
        })
        .expect("create_session");
    let op1 = harness
        .run_turn(
            &session,
            RunTurnRequest {
                input_ref: ArtifactRef::derive("turn-1-input"),
                bounded_turns: 1,
                deadline_ms: 180_000,
            },
        )
        .expect("run_turn");
    let state1 = poll_terminal(&mut harness, &op1, Duration::from_secs(240));
    let events1 = harness.read_events(&op1).expect("read_events");
    eprintln!("turn1 state: {}", state1.label());
    for event in &events1 {
        eprintln!("turn1 event: {}", event.render());
    }
    assert!(
        state1.is_terminal(),
        "turn1 must reach a terminal state, got {}",
        state1.label()
    );
    assert!(
        events1
            .iter()
            .any(|e| matches!(e, Event::TurnCompleted { .. } | Event::TurnFailed { .. })),
        "turn1 events must include a terminal event: {events1:?}"
    );
    assert!(
        events1
            .iter()
            .any(|e| matches!(e, Event::UsageSnapshot { .. })),
        "turn1 events must include a usage snapshot: {events1:?}"
    );

    // --- second session + cancelled turn -----------------------------------
    let session2 = harness
        .create_session(CreateSessionRequest {
            role: "builder".to_string(),
            model: "role-default".to_string(),
            prompts_ref: ArtifactRef::derive("builder-prompts").to_string(),
            handoff_refs: vec![],
            tool_policy_ref: ArtifactRef::derive("tool-policy").to_string(),
        })
        .expect("create_session cancel");
    let op2 = harness
        .run_turn(
            &session2,
            RunTurnRequest {
                input_ref: ArtifactRef::derive("cancel-turn-input"),
                bounded_turns: 1,
                deadline_ms: 180_000,
            },
        )
        .expect("run_turn cancel");
    let ack = harness.cancel_operation(&op2).expect("cancel_operation");
    eprintln!(
        "cancel ack: acknowledged={} terminated={} detail={}",
        ack.acknowledged, ack.terminated, ack.detail
    );
    let state2 = poll_terminal(&mut harness, &op2, Duration::from_secs(120));
    eprintln!("turn2 state: {}", state2.label());
    assert!(
        state2.is_terminal(),
        "cancelled turn must reach a terminal state, got {}",
        state2.label()
    );

    let _ = harness.close_session(&session);
    let _ = harness.close_session(&session2);

    // --- stop attempt: process evidence + no orphans -----------------------
    let evidence = harness.stop_attempt(&attempt).expect("stop_attempt");
    eprintln!("stop evidence: {}", evidence.detail);
    assert!(
        evidence.process_group_gone,
        "process group must be gone: {}",
        evidence.detail
    );
    assert!(!pid_alive(root_pid), "server pid {root_pid} survived stop");
    let survivors = group_members(root_pid);
    assert!(
        survivors.is_empty(),
        "no confined process may survive stop; found {survivors:?}"
    );

    // --- user OpenCode state untouched -------------------------------------
    assert_eq!(
        mtime(&user_config),
        before_config,
        "user ~/.config/opencode was modified"
    );
    assert_eq!(
        mtime(&user_data),
        before_data,
        "user ~/.local/share/opencode was modified"
    );

    eprintln!("live smoke PASS: root {}", root.display());
}
