//! `SandboxApi` — confined process lifecycle.
//!
//! Process launch always passes through this contract. Termination signals
//! descendants explicitly (macOS may reject a group signal) and returns
//! `StopEvidence` so cleanup is proven, not inferred.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::ContractError;
use crate::harness::StopEvidence;
use crate::ids::AttemptId;

/// A confined launch request.
///
/// `profile` is the filesystem path of the rendered sandbox profile (e.g. a
/// Seatbelt `worker.sb`). The four `attempt_dir`/`protected_dir`/`sock_dir`/
/// `extra_ro` fields are the values substituted into the profile's parameters;
/// the implementation supplies every one of them and refuses to launch when the
/// profile is missing or references an unknown parameter (fail-closed).
///
/// `env` is the complete child environment. The caller owns attempt-scoping:
/// `TMPDIR` and `XDG_*` must point inside `attempt_dir`, because the profile
/// denies the global temp dir and the user's real XDG directories.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub profile: String,
    pub attempt: AttemptId,
    /// Attempt-owned read/write root (Seatbelt `ATTEMPT` parameter).
    pub attempt_dir: String,
    /// Protected-tests root, readable but not writable (Seatbelt `PROTECTED`).
    pub protected_dir: String,
    /// Parent-owned Unix-socket directory (Seatbelt `SOCKDIR`).
    pub sock_dir: String,
    /// Extra read-only root, e.g. the harness install tree (Seatbelt `EXTRA_RO`).
    pub extra_ro: String,
}

/// Opaque handle to a launched confined child.
///
/// `pid` is the process id of the confined leader; `pgid` is its process group
/// (equal to `pid` because the launcher makes the child a group leader so
/// descendants can be signalled as a group). `started_at_unix_ms` is the
/// wall-clock launch time used for reconciliation evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxChild {
    pub pid: u32,
    pub pgid: u32,
    pub attempt: AttemptId,
    pub started_at_unix_ms: u64,
}

/// Sandbox contract: launch, liveness, and evidence-backed termination.
pub trait SandboxApi {
    /// Launch a confined child process.
    fn launch(&mut self, spec: LaunchSpec) -> Result<SandboxChild, ContractError>;

    /// Whether the child (and its descendants) is still alive.
    fn alive(&self, child: &SandboxChild) -> Result<bool, ContractError>;

    /// Terminate the child and descendants, returning cleanup evidence.
    fn terminate(&mut self, child: &SandboxChild) -> Result<StopEvidence, ContractError>;
}
