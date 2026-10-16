//! Fake [`ProcessApi`] doubles for B0.5 ownership tests.
//!
//! [`FakeLauncher`] performs no real process work: `launch` writes a durable
//! "live" marker under `<run>/process/<pid>.live` (the fixture's live
//! process-group-leader record), `alive` checks that marker, and `terminate`
//! removes it and writes `StopEvidence` to `<run>/process/<pid>.stop.json`.
//!
//! Because the markers live on disk, a parent recovery process observes a
//! worker launched by a crashed child: the survivor is "alive" until reclaimed,
//! and termination evidence survives independently of either process.
//!
//! [`GoneLauncher`] reports its processes as already gone (termination is always
//! confirmed); [`StubbornLauncher`] reports a live survivor that cannot be
//! confirmed gone.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use agalma_contracts::error::ContractError;
use agalma_contracts::harness::StopEvidence;
use agalma_contracts::ids::AttemptId;
use agalma_contracts::{ProcessApi, ProcessChild, ProcessSpec};

/// Monotonic fake pid source (process-local; markers are the durable truth).
static NEXT_PID: AtomicU32 = AtomicU32::new(2000);

/// A filesystem-backed fake process launcher rooted at a run directory.
pub struct FakeLauncher {
    root: PathBuf,
}

impl FakeLauncher {
    /// Create a fake launcher under `root` (the isolated run dir).
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let _ = fs::create_dir_all(root.join("process"));
        FakeLauncher { root }
    }

    fn process_dir(&self) -> PathBuf {
        self.root.join("process")
    }

    fn live_path(&self, pid: u32) -> PathBuf {
        self.process_dir().join(format!("{pid}.live"))
    }

    fn stop_path(&self, pid: u32) -> PathBuf {
        self.process_dir().join(format!("{pid}.stop.json"))
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

    /// Launch a fake child (no real process) with the given attempt.
    pub fn spawn(&mut self, attempt: u32) -> ProcessChild {
        let pid = NEXT_PID.fetch_add(1, Ordering::SeqCst);
        let child = ProcessChild {
            pid,
            pgid: pid,
            attempt: AttemptId::derive(attempt),
            started_at_unix_ms: 1,
        };
        // `ProcessChild` is intentionally not a wire type, so the durable marker
        // records just the pid; liveness reads marker existence.
        fs::write(self.live_path(pid), pid.to_string()).expect("write live marker");
        child
    }

    /// Simulate a worker dying on its own (removes the live marker).
    pub fn kill_marker(&self, pid: u32) {
        let _ = fs::remove_file(self.live_path(pid));
    }
}

impl ProcessApi for FakeLauncher {
    fn launch(&mut self, spec: ProcessSpec) -> Result<ProcessChild, ContractError> {
        Ok(self.spawn(attempt_of(&spec.attempt)))
    }

    fn alive(&self, child: &ProcessChild) -> Result<bool, ContractError> {
        Ok(self.live_path(child.pid).exists())
    }

    fn terminate(&mut self, child: &ProcessChild) -> Result<StopEvidence, ContractError> {
        let _ = fs::remove_file(self.live_path(child.pid));
        let evidence = StopEvidence {
            process_group_gone: true,
            detail: format!("fake launcher terminated process group {}", child.pgid),
        };
        fs::write(
            self.stop_path(child.pid),
            serde_json::to_string(&evidence).expect("serialize evidence"),
        )
        .expect("write stop evidence");
        Ok(evidence)
    }
}

/// A launcher whose processes are always already gone (confirms termination).
pub struct GoneLauncher;

impl ProcessApi for GoneLauncher {
    fn launch(&mut self, _spec: ProcessSpec) -> Result<ProcessChild, ContractError> {
        Err(ContractError::UnsupportedCapability(
            "gone launcher cannot launch".into(),
        ))
    }

    fn alive(&self, _child: &ProcessChild) -> Result<bool, ContractError> {
        Ok(false)
    }

    fn terminate(&mut self, _child: &ProcessChild) -> Result<StopEvidence, ContractError> {
        Ok(StopEvidence {
            process_group_gone: true,
            detail: "already gone".to_string(),
        })
    }
}

/// A launcher whose process stays alive and cannot be confirmed gone, so a
/// cancellation with a recorded worker is `Settling` (unconfirmed termination).
pub struct StubbornLauncher;

impl ProcessApi for StubbornLauncher {
    fn launch(&mut self, _spec: ProcessSpec) -> Result<ProcessChild, ContractError> {
        Err(ContractError::UnsupportedCapability(
            "stubborn launcher cannot launch".into(),
        ))
    }

    fn alive(&self, _child: &ProcessChild) -> Result<bool, ContractError> {
        Ok(true)
    }

    fn terminate(&mut self, _child: &ProcessChild) -> Result<StopEvidence, ContractError> {
        Ok(StopEvidence {
            process_group_gone: false,
            detail: "stubborn survivor".to_string(),
        })
    }
}

/// A minimal inert [`ProcessSpec`] for the fake launcher (`root` is the run dir).
pub fn spec(root: &Path) -> ProcessSpec {
    let root = root.to_string_lossy().to_string();
    ProcessSpec {
        program: "/bin/false".to_string(),
        args: Vec::new(),
        cwd: root,
        env: Default::default(),
        attempt: AttemptId::derive(1),
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
