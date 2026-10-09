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
/// `profile` names the rendered sandbox profile (e.g. a Seatbelt profile);
/// `env` is the complete child environment (attempt-scoped `TMPDIR`/`XDG_*`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub profile: String,
    pub attempt: AttemptId,
}

/// Opaque handle to a launched confined child.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxChild {
    pub pid: u32,
    pub attempt: AttemptId,
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
