//! Property tests for [`agalma_process::LocalLauncher`].
//!
//! Deterministic and bounded: small fixed process counts, fixed sleep
//! durations, no generated randomness. The assertions use independent OS tools
//! (`/bin/kill`, `/usr/bin/pgrep`) rather than the launcher internals, so they
//! fail if the process-group teardown is wrong.
//!
//! Cases:
//! 1. concurrency — 3 concurrent sleepers, all alive, each terminated with
//!    `process_group_gone`.
//! 2. env separation — only the supplied env reaches the child.
//! 3. tree kill — terminating a shell kills its sleeping grandchild.
//! 4. terminate-from-pid-only — a handle rebuilt from `pid`/`pgid` alone still
//!    terminates.
//! 5. one-shot timeout — `run` on a sleeper returns `KnownFailure` and leaves no
//!    survivor.
//! 6. alive-after-exit — `alive` is false once a short child exits.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use agalma_contracts::{AttemptId, ContractError, ProcessApi, ProcessChild, ProcessSpec};
use agalma_process::LocalLauncher;

/// A long-running child with a distinct attempt id.
fn sleeper(attempt: u32) -> ProcessSpec {
    ProcessSpec {
        program: "/bin/sleep".to_string(),
        args: vec!["30".to_string()],
        cwd: "/".to_string(),
        env: BTreeMap::new(),
        attempt: AttemptId::derive(attempt),
    }
}

/// Pids in process group `pgid` (independent of the launcher).
fn group_pids(pgid: u32) -> Vec<u32> {
    let out = std::process::Command::new("/usr/bin/pgrep")
        .arg("-g")
        .arg(pgid.to_string())
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// Pids whose full command line contains `marker`.
fn pids_matching(marker: &str) -> Vec<u32> {
    let out = std::process::Command::new("/usr/bin/pgrep")
        .arg("-f")
        .arg(marker)
        .output()
        .expect("run pgrep -f");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// Whether `pid` names a live process (independent of the launcher).
fn os_alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Case 1: three concurrent sleepers all terminate with the group gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_concurrent_children_terminate() {
    let mut launcher = LocalLauncher::new();
    let mut children = Vec::new();
    for i in 0..3u32 {
        let child = launcher.launch(sleeper(100 + i)).expect("launch");
        assert_eq!(child.attempt, AttemptId::derive(100 + i));
        assert_eq!(child.pgid, child.pid, "child must be a group leader");
        assert!(launcher.alive(&child).expect("alive"));
        children.push(child);
    }
    // Distinct attempts (and pids) for three concurrent children.
    assert_eq!(children[0].attempt, AttemptId::derive(100));
    assert_eq!(children[1].attempt, AttemptId::derive(101));
    assert_eq!(children[2].attempt, AttemptId::derive(102));

    for child in &children {
        let evidence = launcher.terminate(child).expect("terminate");
        assert!(
            evidence.process_group_gone,
            "pid {}: {}",
            child.pid, evidence.detail
        );
        assert!(!launcher.alive(child).expect("alive after terminate"));
    }
}

/// Case 2: the child sees exactly the supplied env; HOME/TMPDIR are absent.
#[test]
fn child_env_is_exactly_supplied() {
    let mut launcher = LocalLauncher::new();
    let mut env = BTreeMap::new();
    env.insert("AGALMA_IB2_MARKER".to_string(), "present".to_string());
    let spec = ProcessSpec {
        program: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            "printf '%s\\n' \"$AGALMA_IB2_MARKER\"; \
             printf 'HOME=%s\\n' \"${HOME-<unset>}\"; \
             printf 'TMPDIR=%s\\n' \"${TMPDIR-<unset>}\""
                .to_string(),
        ],
        cwd: "/".to_string(),
        env,
        attempt: AttemptId::derive(200),
    };
    let out = launcher.run(&spec, Duration::from_secs(10)).expect("run");
    assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "present\nHOME=<unset>\nTMPDIR=<unset>\n");
}

/// Case 3: terminating the parent shell kills its sleeping grandchild.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_kills_grandchild() {
    let mut launcher = LocalLauncher::new();
    let spec = ProcessSpec {
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), "sleep 30 & wait".to_string()],
        cwd: "/".to_string(),
        env: BTreeMap::new(),
        attempt: AttemptId::derive(300),
    };
    let child = launcher.launch(spec).expect("launch");

    // Let the shell fork the background sleeper into the same group.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let members = group_pids(child.pgid);
    let grandchild = members
        .iter()
        .copied()
        .find(|p| *p != child.pid)
        .expect("a grandchild must share the group");
    assert!(os_alive(grandchild), "grandchild {grandchild} should be alive");

    let evidence = launcher.terminate(&child).expect("terminate");
    assert!(evidence.process_group_gone, "detail: {}", evidence.detail);
    assert!(!os_alive(grandchild), "grandchild {grandchild} survived");
}

/// Case 4: a handle reconstructed from pid/pgid alone terminates the tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_from_pid_only() {
    let mut owner = LocalLauncher::new();
    let spec = sleeper(400);
    let attempt = spec.attempt.clone();
    let launched = owner.launch(spec).expect("launch");

    // Post-restart shape: a fresh launcher owns no handle for this child.
    let reconstructed = ProcessChild {
        pid: launched.pid,
        pgid: launched.pgid,
        attempt,
        started_at_unix_ms: 0,
    };
    let mut stranger = LocalLauncher::new();
    let evidence = stranger.terminate(&reconstructed).expect("terminate");
    assert!(evidence.process_group_gone, "detail: {}", evidence.detail);
    // The owner reaps the direct child; liveness then reports it dead.
    assert!(!owner.alive(&launched).expect("alive"));
    assert!(!os_alive(launched.pid));
}

/// Case 5: a one-shot run past its timeout returns `KnownFailure` and leaves no
/// survivor.
#[test]
fn one_shot_timeout_leaves_no_survivor() {
    const MARKER: &str = "agalma-ib2-timeout-marker";
    let mut launcher = LocalLauncher::new();
    let spec = ProcessSpec {
        // `; :` keeps the shell from `exec`-ing `sleep`, so the leader keeps the
        // marker in its argv and is observable via `pgrep -f`.
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), "sleep 30; :".to_string(), MARKER.to_string()],
        cwd: "/".to_string(),
        env: BTreeMap::new(),
        attempt: AttemptId::derive(500),
    };
    let err = launcher
        .run(&spec, Duration::from_millis(300))
        .expect_err("run must time out");
    assert!(
        matches!(err, ContractError::KnownFailure(_)),
        "expected KnownFailure, got {err:?}"
    );
    assert!(
        pids_matching(MARKER).is_empty(),
        "timeout left a survivor: {:?}",
        pids_matching(MARKER)
    );
}

/// Case 6: `alive` is false once a short child exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alive_is_false_after_exit() {
    let mut launcher = LocalLauncher::new();
    let spec = ProcessSpec {
        program: "/usr/bin/true".to_string(),
        args: vec![],
        cwd: "/".to_string(),
        env: BTreeMap::new(),
        attempt: AttemptId::derive(600),
    };
    let child = launcher.launch(spec).expect("launch");

    let deadline = Instant::now() + Duration::from_secs(5);
    while launcher.alive(&child).expect("alive") {
        assert!(
            Instant::now() < deadline,
            "child {} stayed alive after exit",
            child.pid
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
