//! Fake [`SandboxApi`] double for B0.5 ownership tests.
//!
//! The double performs no real process work: `launch` writes a durable "live"
//! marker under `<run>/sandbox/<pid>.live` (the fixture's live
//! process-group-leader record), `alive` checks that marker, and `terminate`
//! removes it and writes `StopEvidence` to `<run>/sandbox/<pid>.stop.json`.
//!
//! Because the markers live on disk, a parent recovery process observes a
//! worker launched by a crashed child: the survivor is "alive" until reclaimed,
//! and termination evidence survives independently of either process.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use agalma_contracts::error::ContractError;
use agalma_contracts::harness::StopEvidence;
use agalma_contracts::ids::AttemptId;
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi, SandboxChild};

/// Monotonic fake pid source (process-local; markers are the durable truth).
static NEXT_PID: AtomicU32 = AtomicU32::new(2000);

/// A filesystem-backed fake sandbox rooted at a run directory.
pub struct FakeSandbox {
    root: PathBuf,
}

impl FakeSandbox {
    /// Create a fake sandbox under `root` (the isolated run dir).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let _ = fs::create_dir_all(root.join("sandbox"));
        FakeSandbox { root }
    }

    fn sandbox_dir(&self) -> PathBuf {
        self.root.join("sandbox")
    }

    fn live_path(&self, pid: u32) -> PathBuf {
        self.sandbox_dir().join(format!("{pid}.live"))
    }

    fn stop_path(&self, pid: u32) -> PathBuf {
        self.sandbox_dir().join(format!("{pid}.stop.json"))
    }

    /// Whether a live marker exists for `pid`.
    pub fn pid_live(&self, pid: u32) -> bool {
        self.live_path(pid).exists()
    }

    /// Recorded stop evidence for `pid`, if the worker was terminated.
    pub fn stop_evidence(&self, pid: u32) -> Option<StopEvidence> {
        let text = fs::read_to_string(self.stop_path(pid)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Launch a fake confined child (no real process) with the given attempt.
    pub fn spawn(&mut self, attempt: u32) -> SandboxChild {
        let pid = NEXT_PID.fetch_add(1, Ordering::SeqCst);
        let child = SandboxChild {
            pid,
            pgid: pid,
            attempt: AttemptId::derive(attempt),
            started_at_unix_ms: 1,
        };
        fs::write(
            self.live_path(pid),
            serde_json::to_string(&child).expect("serialize child"),
        )
        .expect("write live marker");
        child
    }

    /// Simulate a worker dying on its own (removes the live marker).
    pub fn kill_marker(&self, pid: u32) {
        let _ = fs::remove_file(self.live_path(pid));
    }
}

impl SandboxApi for FakeSandbox {
    fn launch(&mut self, spec: LaunchSpec) -> Result<SandboxChild, ContractError> {
        Ok(self.spawn(attempt_of(&spec.attempt)))
    }

    fn alive(&self, child: &SandboxChild) -> Result<bool, ContractError> {
        Ok(self.live_path(child.pid).exists())
    }

    fn terminate(&mut self, child: &SandboxChild) -> Result<StopEvidence, ContractError> {
        let _ = fs::remove_file(self.live_path(child.pid));
        let evidence = StopEvidence {
            process_group_gone: true,
            detail: format!("fake sandbox terminated process group {}", child.pgid),
        };
        fs::write(
            self.stop_path(child.pid),
            serde_json::to_string(&evidence).expect("serialize evidence"),
        )
        .expect("write stop evidence");
        Ok(evidence)
    }
}

/// A minimal inert [`LaunchSpec`] for the fake sandbox (`root` is the run dir).
pub fn spec(root: &Path) -> LaunchSpec {
    let root = root.to_string_lossy().to_string();
    LaunchSpec {
        program: "/bin/false".to_string(),
        args: Vec::new(),
        cwd: root.clone(),
        env: Default::default(),
        profile: "fake".to_string(),
        attempt: AttemptId::derive(1),
        attempt_dir: root.clone(),
        protected_dir: root.clone(),
        sock_dir: root,
        extra_ro_roots: Vec::new(),
        extra_rw_roots: Vec::new(),
    }
}

/// Parse the numeric suffix of an `attempt:<n>` id.
fn attempt_of(attempt: &AttemptId) -> u32 {
    attempt
        .as_str()
        .rsplit(':')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}
