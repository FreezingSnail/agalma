//! Handle-less terminate regression (`docs/sandbox-removal-design.md` §10).
//!
//! Cross-process ownership: with one `LocalLauncher` per component instead of a
//! single shared sandbox server, a process that launched a child may be gone (a
//! conductor restart, or a fresh process reclaiming a survivor) before the child
//! is terminated. Termination must therefore work from `pid`/`pgid` alone, with
//! no owned handle, and must still prove the whole process group — leader plus
//! descendants — is gone.
//!
//! Deterministic and bounded: one leader, one descendant, fixed sleeps, no
//! generated randomness. The assertions use independent OS tools
//! (`/bin/kill`, `/usr/bin/pgrep`) rather than the launcher internals, so they
//! fail if the group teardown is wrong.

use std::collections::BTreeMap;
use std::time::Duration;

use agalma_contracts::{AttemptId, ProcessApi, ProcessChild, ProcessSpec};
use agalma_process::LocalLauncher;

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

/// Whether `pid` names a live process (independent of the launcher).
fn os_alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The owner loses its handle, then a fresh launcher terminates the tree from
/// `pid`/`pgid` alone; the whole group must be gone and no recorded pid alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_launcher_terminates_child_without_owned_handle() {
    let attempt = AttemptId::derive(900);

    // The owner launches a long-running leader that forks a sleeping descendant
    // into the same process group, then waits.
    let mut owner = LocalLauncher::new();
    let spec = ProcessSpec {
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), "sleep 30 & wait".to_string()],
        cwd: "/".to_string(),
        env: BTreeMap::new(),
        attempt: attempt.clone(),
    };
    let launched = owner.launch(spec).expect("launch");
    assert_eq!(launched.pgid, launched.pid, "leader must own the group");

    // Let the shell fork the descendant into the group.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let recorded = group_pids(launched.pgid);
    assert!(
        recorded.contains(&launched.pid),
        "leader {} missing from its own group {recorded:?}",
        launched.pid
    );
    assert!(
        recorded.len() >= 2,
        "expected a descendant in the group, saw {recorded:?}"
    );

    // Handle loss: the owning launcher is dropped with no terminate call, so the
    // child keeps running and only its pid/pgid survive the boundary.
    drop(owner);

    // A fresh launcher owns no handle for this child and reconstructs it from
    // pid/pgid alone, exactly as after a restart.
    let reconstructed = ProcessChild {
        pid: launched.pid,
        pgid: launched.pgid,
        attempt,
        started_at_unix_ms: 0,
    };
    let mut stranger = LocalLauncher::new();
    let evidence = stranger.terminate(&reconstructed).expect("terminate");
    assert!(evidence.process_group_gone, "detail: {}", evidence.detail);

    // No recorded pid (leader or descendant) survives, and the group is empty.
    for pid in &recorded {
        assert!(!os_alive(*pid), "pid {pid} survived termination");
    }
    assert!(
        group_pids(launched.pgid).is_empty(),
        "group {} still populated: {:?}",
        launched.pgid,
        group_pids(launched.pgid)
    );
}
