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
/// Seatbelt `worker.sb`). The `attempt_dir`/`protected_dir`/`sock_dir` fields
/// plus up to three entries of `extra_ro_roots` are the values substituted into
/// the profile's parameters; the implementation supplies every one of them and
/// refuses to launch when the profile is missing or references an unknown
/// parameter (fail-closed). `extra_ro_roots` may hold fewer than three roots;
/// the launcher must still render every slot (unused slots are inert).
///
/// `extra_rw_roots` grants writable roots beyond `attempt_dir` (Seatbelt
/// `EXTRA_RW`), e.g. a target repository a session must edit while its own
/// runtime state stays in a conductor-owned `attempt_dir`.
///
/// `env` is the complete child environment. The caller owns attempt-scoping:
/// `TMPDIR` and `XDG_*` must point inside `attempt_dir`, because the profile
/// denies the global temp dir and the user's real XDG directories. They must
/// not point inside a writable workspace root: harness runtime state (e.g. a
/// git snapshot store) inside the worktree it watches feeds back on itself.
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
    /// Extra read-only roots, e.g. the harness install tree and toolchain homes
    /// (Seatbelt `EXTRA_RO`, `EXTRA_RO2`, `EXTRA_RO3` in order). At most three
    /// are honoured; the launcher always renders all three slots, padding
    /// unused ones with an inert value.
    pub extra_ro_roots: Vec<String>,
    /// Extra read/write roots (Seatbelt `EXTRA_RW`). At most one is honoured;
    /// the launcher always renders the slot, padding it with an inert value.
    /// Absent on the wire means none (older clients stay compatible).
    #[serde(default)]
    pub extra_rw_roots: Vec<String>,
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
