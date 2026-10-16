//! `ProcessApi` — local process lifecycle.
//!
//! This is the in-process replacement for the confined-sandbox contract
//! (`docs/sandbox-removal-design.md` §4.1): the same launch/liveness/terminate
//! shape as `SandboxApi`, with the confinement-specific fields removed. A
//! process is launched as its own process-group leader so descendants can be
//! signalled and verified as a tree; termination returns [`StopEvidence`] so
//! cleanup is proven, not inferred.
//!
//! The module is pure types + trait (no I/O, no vendor types, no wire derives).

use std::collections::BTreeMap;

use crate::error::ContractError;
use crate::harness::StopEvidence;
use crate::ids::AttemptId;

/// A local process launch request.
///
/// `env` is the complete child environment: the implementation clears the
/// inherited environment and applies exactly these entries. The caller owns any
/// attempt-scoping (e.g. pointing `TMPDIR`/`XDG_*` at an attempt directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    /// Reconciliation key carried through the launch handle.
    pub attempt: AttemptId,
}

/// Opaque handle to a launched child.
///
/// `pid` is the process id of the child, which the launcher makes a
/// process-group leader, so `pgid == pid` and every descendant that does not
/// call `setsid` shares the group. `started_at_unix_ms` is the wall-clock launch
/// time used for reconciliation evidence. The handle carries everything needed
/// to terminate the tree after a restart, without the original owned handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessChild {
    pub pid: u32,
    pub pgid: u32,
    pub attempt: AttemptId,
    pub started_at_unix_ms: u64,
}

/// Process contract: launch, liveness, and evidence-backed termination.
pub trait ProcessApi {
    /// Launch a child process as a process-group leader.
    fn launch(&mut self, spec: ProcessSpec) -> Result<ProcessChild, ContractError>;

    /// Whether the child (and its group) is still alive.
    fn alive(&self, child: &ProcessChild) -> Result<bool, ContractError>;

    /// Terminate the child and descendants, returning cleanup evidence.
    fn terminate(&mut self, child: &ProcessChild) -> Result<StopEvidence, ContractError>;
}
